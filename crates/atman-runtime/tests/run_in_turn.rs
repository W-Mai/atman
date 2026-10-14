use atman_runtime::AtmanPayload;
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use atman_rt::parse_file;
use atman_runtime::Executor;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{LlmCallStatus, NodeEvent, Observable};
type TurnId = atman_rt::TurnId<atman_runtime::event::AtmanUuid>;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, StopReason,
    TokenUsage,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct EmptyThenGoodProvider {
    calls: AtomicU32,
}

struct MissingIntentProvider {
    calls: AtomicU32,
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
}

#[derive(Default)]
struct FinalAnswerProvider {
    plain_stop: Option<StopReason>,
    tools_before_final: bool,
    tool_fails: bool,
    calls: AtomicU32,
}

impl FinalAnswerProvider {
    fn response(&self, req: &LlmRequest) -> AssistantMessage {
        let turn_id = req
            .messages
            .first()
            .map(|message| message.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.tools_before_final && matches!(call, 2 | 3) {
            let reminder = req.messages.last().unwrap();
            assert_eq!(reminder.role, MessageRole::User);
            assert!(
                reminder.text_concat().starts_with("Agent loop control"),
                "request messages: {:#?}",
                req.messages
            );
            assert!(reminder.text_concat().contains("final.answer"));
            assert!(req.messages.iter().any(|message| {
                message.role == MessageRole::Assistant
                    && message.origin != MessageOrigin::FinalAnswer
                    && message.text_concat() == "Done."
            }));
            assert!(req.messages.iter().any(|message| message.parts.iter().any(
                |part| matches!(part, MessagePart::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == "read-rules" && *is_error == self.tool_fails)
            )));
        }
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts: if self.tools_before_final && call == 0 {
                    vec![MessagePart::ToolUse {
                        id: "read-rules".into(),
                        name: "rule.fetch".into(),
                        input: serde_json::json!({}),
                        intent: (!self.tool_fails).then(|| {
                            atman_runtime::message::ToolCallIntent::new("Read rules").unwrap()
                        }),
                    }]
                } else if self.plain_stop.is_some() || (self.tools_before_final && call != 3) {
                    vec![MessagePart::Text {
                        text: "Done.".into(),
                    }]
                } else {
                    vec![MessagePart::ToolUse {
                        id: "final-1".into(),
                        name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
                        input: serde_json::json!({"message": "Done."}),
                        intent: None,
                    }]
                },
                turn_id,
                origin: MessageOrigin::User,
            },
            stop_reason: self.plain_stop.clone().unwrap_or(StopReason::End),
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "final-model".into(),
            response_id: None,
        }
    }
}

impl Provider for FinalAnswerProvider {
    fn name(&self) -> &str {
        "final-model"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { Ok(self.response(&req)) })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let response = self.response(&req);
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(response)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

impl MissingIntentProvider {
    fn response(&self) -> AssistantMessage {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let parts = if call == 0 {
            vec![MessagePart::ToolUse {
                id: format!("call_{call}"),
                name: "fs.read".into(),
                input: serde_json::json!({"path": "README.md"}),
                intent: None,
            }]
        } else {
            vec![MessagePart::Text {
                text: "observed tool failure".into(),
            }]
        };
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "intent-model".into(),
            response_id: None,
        }
    }
}

impl Provider for MissingIntentProvider {
    fn name(&self) -> &str {
        "intent-model"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(req.messages);
            Ok(self.response())
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        self.requests.lock().unwrap().push(req.messages);
        let response = self.response();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(response)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

impl EmptyThenGoodProvider {
    fn response(&self) -> AssistantMessage {
        let parts = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Vec::new()
        } else {
            vec![MessagePart::Text { text: "ok".into() }]
        };
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "empty-then-good".into(),
            response_id: None,
        }
    }
}

impl Provider for EmptyThenGoodProvider {
    fn name(&self) -> &str {
        "empty-then-good"
    }

    fn call<'a>(&'a self, _req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { Ok(self.response()) })
    }

    fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
        let response = self.response();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(response)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

#[tokio::test]
async fn run_in_turn_appends_assistant_message_to_session() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> message {
    return llm.call(model: "mock", prompt: "hi", context: "session", tools: ["final.answer"])
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    atman_runtime::tools::register_tier_zero(&executor.tools);
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("hello world".into())),
    ));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "please respond"));

    let out = executor
        .run_in_turn(
            &file,
            "ask",
            vec![],
            Some(turn_id.clone()),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();

    assert!(
        matches!(&out, Value::Host(AtmanPayload::Message(message)) if message.text_concat() == "hello world")
    );

    let msgs = session.messages();
    let ordinary: Vec<_> = msgs
        .iter()
        .filter(|message| {
            !message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ContextRecord(_)))
        })
        .collect();
    assert_eq!(
        ordinary.len(),
        2,
        "root assistant must stay in session context"
    );
    assert_eq!(ordinary[0].role, MessageRole::User);
    assert_eq!(ordinary[1].role, MessageRole::Assistant);
    assert_ne!(ordinary[1].origin, MessageOrigin::FinalAnswer);
    assert_eq!(ordinary[1].text_concat(), "hello world");

    let has_correlated_assistant = session.sink().snapshot().iter().any(|event| {
        matches!(
            event,
            atman_runtime::Event::AssistantMsg {
                turn_id: t,
                flow_run_id: Some(_),
                message,
            } if *t == turn_id && message.text_concat() == "hello world"
        )
    });
    assert!(has_correlated_assistant);
}

#[tokio::test]
async fn root_final_answer_without_provider_intent_is_persisted_after_explicit_acceptance() {
    let _registry = common::ModelRegistryGuard::mock("final-model").await;
    let src = r#"flow ask() -> string {
    reply = llm.call(
        model: "final-model",
        prompt: "finish",
        context: "session",
        tools: ["final.answer"],
    )
    answer = extract_final_answer(reply)
    session.push(finalize_response(reply))
    return answer
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    atman_runtime::tools::register_tier_zero(&executor.tools);
    executor
        .providers
        .register(Arc::new(FinalAnswerProvider::default()));
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert!(matches!(output, Value::Str(answer) if answer == "Done."));
    let assistants = session
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].origin, MessageOrigin::FinalAnswer);
    assert_eq!(assistants[0].text_concat(), "Done.");
    assert_eq!(
        atman_runtime::tools::final_answer::summary(&assistants[0]).as_deref(),
        Some("Done.")
    );
}

#[tokio::test]
async fn managed_final_answer_ends_before_watcher_notifications_start_a_new_turn() {
    assert_managed_final_answer_boundary(Arc::new(FinalAnswerProvider::default()), true).await;
}

#[tokio::test]
async fn managed_plain_response_uses_the_same_final_answer_boundary() {
    assert_managed_final_answer_boundary(
        Arc::new(MockProvider::new("final-model").with_fallback(Value::Str("Done.".into()))),
        true,
    )
    .await;
}

#[tokio::test]
async fn managed_flow_does_not_finalize_truncated_or_cancelled_text() {
    for reason in [StopReason::Length, StopReason::Cancelled] {
        assert_managed_final_answer_boundary(
            Arc::new(FinalAnswerProvider {
                plain_stop: Some(reason),
                ..Default::default()
            }),
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn every_managed_subagent_role_delivers_plain_text_through_final_answer() {
    assert_subagent_final_answer_policy(false).await;
}

#[tokio::test]
async fn every_managed_subagent_role_requires_explicit_final_answer_after_tools() {
    assert_subagent_final_answer_policy(true).await;
}

async fn assert_subagent_final_answer_policy(tools_before_final: bool) {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "final-model",
            "final-model",
            131_072,
            None,
        )]))
        .await;
    let dir = tempfile::tempdir().unwrap();
    atman_runtime::templates::ensure_managed_agent_at(dir.path()).unwrap();
    let mut source = atman_runtime::templates::SUBAGENT_AT.to_string();
    let mut offset = 0;
    while let Some(start) = source[offset..].find("tools: [") {
        let start = offset + start + "tools: [".len();
        let end = start + source[start..].find(']').unwrap();
        let catalog = "\"final.answer\", \"rule.fetch\"";
        source.replace_range(start..end, catalog);
        offset = start + catalog.len();
    }
    let file = parse_file(&source).unwrap();
    let mut executor = Executor::new();
    executor.source_dir = Some(dir.path().join("commands"));
    atman_runtime::tools::register_tier_zero(&executor.tools);
    for entry in [
        "research_loop",
        "verify_loop",
        "implement_loop",
        "review_loop",
    ] {
        let provider = Arc::new(FinalAnswerProvider {
            plain_stop: (!tools_before_final).then_some(StopReason::End),
            tools_before_final,
            ..Default::default()
        });
        executor.providers.register(provider.clone());
        let session = Arc::new(Session::open_ephemeral());
        let turn = TurnId::now();
        session.begin_turn(user_msg(turn.clone(), "finish"));
        let output = executor
            .run_in_turn(
                &file,
                entry,
                vec![
                    ("goal".into(), Value::Str("finish".into())),
                    ("model".into(), Value::Str("final-model".into())),
                    ("max_iter".into(), Value::Int(200)),
                ],
                Some(turn),
                Some(session.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(output, Value::Str(text) if text == "Done."));
        let messages = session.messages();
        let assistants: Vec<_> = messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .collect();
        let expected_calls = if tools_before_final { 4 } else { 1 };
        assert_eq!(provider.calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(assistants.len(), expected_calls as usize, "{entry}");
        assert_eq!(
            assistants.last().unwrap().origin,
            MessageOrigin::FinalAnswer,
            "{entry}"
        );
        assert_eq!(
            assistants
                .iter()
                .filter(|m| m.origin == MessageOrigin::FinalAnswer)
                .count(),
            1
        );
        session.end_turn();
    }
}

#[tokio::test]
async fn spawned_plain_response_is_finalized_once_in_its_own_context() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let executor = Executor::new();
    atman_runtime::tools::register_tier_zero(&executor.tools);
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("Complete answer.".into())),
    ));
    let session = Arc::new(Session::open_ephemeral());
    let turn = TurnId::now();
    session.begin_turn(user_msg(turn.clone(), "start"));
    let file = parse_file(
        r#"
flow child() -> string {
    response = llm.call(model: "mock", context: "session", record: false, output: "response")
    return final.answer(message: response.message)
}
flow parent() -> [string] {
    return fanout [child()]
}
"#,
    )
    .unwrap();
    let result = executor
        .run_in_turn(&file, "parent", vec![], Some(turn), Some(session.clone()))
        .await
        .unwrap();
    assert!(matches!(result, Value::List(items)
        if matches!(items.as_slice(), [Value::Str(answer)] if answer == "Complete answer.")));
    let finals = executor
        .events
        .snapshot()
        .into_iter()
        .filter(|event| {
            matches!(event, atman_runtime::event::Event::AssistantMsg { message, .. }
            if message.origin == MessageOrigin::FinalAnswer)
        })
        .count();
    assert_eq!(finals, 1);
    assert!(
        !session
            .messages()
            .iter()
            .any(|message| message.role == MessageRole::Assistant)
    );
    session.end_turn();
}

#[tokio::test]
async fn managed_tool_turns_require_explicit_final_until_the_next_turn() {
    for source in [
        atman_runtime::templates::AGENT_AT,
        include_str!("../../../examples/agent.at"),
    ] {
        for tool_fails in [false, true] {
            let provider = Arc::new(FinalAnswerProvider {
                tools_before_final: true,
                tool_fails,
                ..Default::default()
            });
            assert_final_answer_boundary(source, provider.clone(), true, 4).await;
            // The new watcher turn has no tool calls and accepts plain text immediately.
            assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
        }
    }
}

async fn assert_managed_final_answer_boundary(provider: Arc<dyn Provider>, finalized: bool) {
    assert_final_answer_boundary(atman_runtime::templates::AGENT_AT, provider, finalized, 1).await;
}

async fn assert_final_answer_boundary(
    source: &str,
    provider: Arc<dyn Provider>,
    finalized: bool,
    first_responses: usize,
) {
    use atman_runtime::memory::goal::{GoalActor, GoalPauseReason};
    use atman_runtime::watch::{WatchEvent, WatchMode, WatchSource};
    use std::time::Duration;

    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("smart", "final-model", 131_072, None),
        common::mock_model("cheap"),
    ]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    atman_runtime::templates::ensure_managed_agent_at(dir.path()).unwrap();
    // Keep the shipped control flow; limit the provider catalog to this test's tools.
    let mut source = source.to_string();
    let tools_start = source.find("tools: [").unwrap() + "tools: [".len();
    let tools_end = tools_start + source[tools_start..].find(']').unwrap();
    source.replace_range(tools_start..tools_end, "\"final.answer\", \"rule.fetch\"");
    let file = parse_file(&source).unwrap();
    let mut executor = Executor::new();
    executor.source_dir = Some(dir.path().join("commands"));
    atman_runtime::tools::register_tier_zero(&executor.tools);
    atman_runtime::tools::register_watch(&executor.tools);
    executor
        .tools
        .register(Arc::new(atman_runtime::tools::memory::MemoryRecentTurns));
    executor.tools.register(Arc::new(
        atman_runtime::tools::memory::MemoryFetchConfessions {
            store: Arc::new(atman_runtime::memory::confession::ConfessionStore::at(
                dir.path(),
            )),
        },
    ));
    executor.providers.register(provider);
    executor
        .providers
        .register(Arc::new(MockProvider::new("cheap").with_fallback(
            Value::Str(r#"{"rule_names":[],"confession_triggers":[]}"#.into()),
        )));
    let session = Arc::new(Session::open_ephemeral());
    executor.tool_ctx.watch_hub = Some(session.watch_hub.clone());
    session.goal_control.set("Finish the task", None).unwrap();
    session
        .goal_control
        .pause(
            GoalActor::Agent,
            GoalPauseReason::Completed,
            "Done".into(),
            None,
        )
        .unwrap();
    for index in 0..9 {
        let source = WatchSource::Bash {
            handle: format!("job-{index}"),
        };
        let id = session.watch_hub.register(
            source.clone(),
            "done".into(),
            WatchMode::Once,
            Duration::from_secs(1),
        );
        session.watch_hub.enqueue_event(WatchEvent {
            watcher_id: id,
            source,
            pattern: "done".into(),
            row: None,
            col: None,
            text: format!("result-{index}"),
            timestamp: chrono::Utc::now(),
            timed_out: false,
            exited: false,
            timeout: Duration::from_secs(1),
        });
    }
    assert!(!session.watch_hub.has_active_watchers());
    let first = TurnId::now();
    session.begin_turn(user_msg(first.clone(), "finish"));
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        executor.run_in_turn(
            &file,
            "agent",
            vec![("user_prompt".into(), Value::Str("finish".into()))],
            Some(first.clone()),
            Some(session.clone()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(output, Value::Str(text) if text == "Done."));
    session.end_turn();
    let messages = session.messages();
    let finals: Vec<_> = messages
        .iter()
        .filter(|m| m.origin == MessageOrigin::FinalAnswer)
        .collect();
    assert_eq!(finals.len(), usize::from(finalized));
    if finalized {
        assert_eq!(finals[0].turn_id, first);
    }
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .count(),
        first_responses
    );
    assert!(!messages.iter().any(|m| m.origin == MessageOrigin::Watcher));

    // An explicit pause must leave the complete event batch available for Resume.
    session
        .goal_control
        .pause(
            GoalActor::User,
            GoalPauseReason::UserRequested,
            "Wait".into(),
            None,
        )
        .unwrap();
    assert!(!atman_runtime::goal::has_next_turn(
        &session.goal_control,
        &session.watch_hub
    ));
    assert!(
        atman_runtime::goal::begin_background_turn(&session)
            .await
            .is_none()
    );
    session.goal_control.resume(GoalActor::User, None).unwrap();
    session
        .goal_control
        .pause(
            GoalActor::Agent,
            GoalPauseReason::Completed,
            "Done".into(),
            None,
        )
        .unwrap();
    assert!(atman_runtime::goal::has_next_turn(
        &session.goal_control,
        &session.watch_hub
    ));
    assert!(session.watch_hub.has_pending_events());
    let message = atman_runtime::goal::begin_background_turn(&session)
        .await
        .unwrap();
    let prompt = message.text_concat();
    let second = message.turn_id;
    assert_eq!(message.origin, MessageOrigin::Watcher);
    let offsets: Vec<_> = (0..9)
        .map(|i| prompt.find(&format!("result-{i}")).unwrap())
        .collect();
    assert!(offsets.windows(2).all(|p| p[0] < p[1]));
    assert!(!atman_runtime::goal::has_next_turn(
        &session.goal_control,
        &session.watch_hub
    ));
    let output = executor
        .run_in_turn(
            &file,
            "agent",
            vec![("user_prompt".into(), Value::Str(prompt))],
            Some(second.clone()),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();
    assert!(matches!(output, Value::Str(text) if text == "Done."));
    let messages = session.messages();
    let finals: Vec<_> = messages
        .iter()
        .filter(|m| m.origin == MessageOrigin::FinalAnswer)
        .collect();
    assert_eq!(finals.len(), 2 * usize::from(finalized));
    if finalized {
        assert_eq!(finals[1].turn_id, second);
    }
    assert_ne!(first, second);
    assert!(!session.goal_control.should_continue());
}

#[tokio::test]
async fn run_without_turn_does_not_touch_session() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("no session".into())),
    ));

    let out = executor.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "no session"));
}

#[tokio::test]
async fn assistant_msg_event_carries_flow_run_id() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> message {
    return llm.call(model: "mock", prompt: "hi", context: "session")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("ok".into())),
    ));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    executor
        .run_in_turn(
            &file,
            "ask",
            vec![],
            Some(turn_id.clone()),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();

    let events = session.sink().snapshot();
    let has_correlated_assistant = events.iter().any(|e| {
        matches!(
            e,
            atman_runtime::Event::AssistantMsg {
                turn_id: t,
                flow_run_id: Some(_),
                ..
            } if *t == turn_id
        )
    });
    assert!(
        has_correlated_assistant,
        "assistant_msg must carry flow_run_id when run_in_turn"
    );
}

#[tokio::test]
async fn empty_assistant_message_retries_without_entering_session_history() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "empty-model",
            "empty-then-good",
            8_192,
            None,
        )]))
        .await;
    let file = parse_file(
        r#"flow ask() -> message {
    return llm.call(model: "empty-model", prompt: "hi", context: "session", retry: 1)
}
"#,
    )
    .unwrap();
    let executor = Executor::new();
    let provider = Arc::new(EmptyThenGoodProvider {
        calls: AtomicU32::new(0),
    });
    executor.providers.register(provider.clone());
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        executor.run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone())),
    )
    .await
    .expect("empty response retry must complete")
    .unwrap();
    session.end_turn();

    assert!(
        matches!(&output, Value::Host(AtmanPayload::Message(message)) if message.text_concat() == "ok")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let messages = session.messages();
    let assistant_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].text_concat(), "ok");
    let statuses = executor
        .events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::LlmCall { status, .. } => Some(status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        statuses.as_slice(),
        [LlmCallStatus::Errored { message }, LlmCallStatus::Ok]
            if message.contains("empty assistant message")
    ));
}

#[tokio::test]
async fn exhausted_llm_failure_releases_context_lock() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "empty-model",
            "empty-then-good",
            8_192,
            None,
        )]))
        .await;
    let file = parse_file(
        r#"flow ask() -> message {
        return llm.call(model: "empty-model", prompt: "hi", context: "session", retry: 0)
    }"#,
    )
    .unwrap();
    let executor = Executor::new();
    let provider = Arc::new(EmptyThenGoodProvider {
        calls: AtomicU32::new(0),
    });
    executor.providers.register(provider.clone());
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        executor.run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone())),
    )
    .await
    .expect("failed call must not wait on its own context lock")
    .unwrap_err();
    assert!(error.to_string().contains("empty assistant message"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let guard = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.acquire_compact_lock(),
    )
    .await
    .expect("context lock must remain available after failure");
    drop(guard);
    session.end_turn();
}

#[tokio::test]
async fn missing_tool_intent_returns_a_visible_tool_failure_without_llm_retry() {
    let _registry = common::ModelRegistryGuard::mock("intent-model").await;
    let file = parse_file(
        r#"flow ask() -> any {
    reply = llm.call(
        model: "intent-model",
        prompt: "read the project documentation",
        context: "session",
        tools: ["fs.read"],
    )
    tool_uses = extract_tool_uses(reply)
    tool_results = dispatch_all(tool_uses)
    session.push(tool_results)
    return llm.call(
        model: "intent-model",
        prompt: "continue after the tool result",
        context: "session",
        tools: ["fs.read"],
    )
}
"#,
    )
    .unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let executor = Executor::with_events(session.sink().clone());
    atman_runtime::tools::register_tier_zero(&executor.tools);
    let provider = Arc::new(MissingIntentProvider {
        calls: AtomicU32::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    executor.providers.register(provider.clone());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        output,
        Value::Host(AtmanPayload::Message(message)) if message.text_concat() == "observed tool failure"
    ));
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        2
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].iter().any(|message| matches!(
            message.parts.as_slice(),
            [MessagePart::ToolResult { content, is_error: true, .. }]
                if content.contains("was not executed")
                    && content.contains("_atman_intent")
                    && content.contains("do not assume this call succeeded")
        )),
        "second request: {:#?}",
        requests[1]
    );
    let statuses = executor
        .events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::LlmCall { status, .. } => Some(status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        statuses.as_slice(),
        [LlmCallStatus::Ok, LlmCallStatus::Ok]
    ));
}
