mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{Event, FlowRunId, NodeEvent, Observable, TurnId};
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage};
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Value, tools};

/// Records the messages each LLM call receives so we can assert that
/// `context: session` actually feeds session history into the provider.
struct RecordingProvider {
    calls: AtomicUsize,
    captured_messages: std::sync::Mutex<Vec<Vec<Message>>>,
    script: Vec<Vec<MessagePart>>,
}

impl RecordingProvider {
    fn new(script: Vec<Vec<MessagePart>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            captured_messages: std::sync::Mutex::new(Vec::new()),
            script,
        }
    }

    fn captured(&self) -> Vec<Vec<Message>> {
        self.captured_messages.lock().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            self.captured_messages
                .lock()
                .unwrap()
                .push(req.messages.clone());
            let parts = self.script.get(idx).cloned().unwrap_or_else(|| {
                vec![MessagePart::Text {
                    text: "[scripted: exhausted]".into(),
                }]
            });
            let turn_id = req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now);
            Ok(AssistantMessage {
                message: Message {
                    role: MessageRole::Assistant,
                    parts,
                    turn_id,
                    origin: MessageOrigin::User,
                },
                stop_reason: StopReason::End,
                token_usage: TokenUsage::default(),
                timing: atman_runtime::provider::CallTiming::default(),
                model: String::new(),
                response_id: None,
            })
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;
        let (tx, events) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        self.captured_messages
            .lock()
            .unwrap()
            .push(req.messages.clone());
        let turn_id = req
            .messages
            .first()
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let parts = self.script.get(idx).cloned().unwrap_or_else(|| {
            vec![MessagePart::Text {
                text: "[scripted: exhausted]".into(),
            }]
        });
        let msg = AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id,
                origin: MessageOrigin::User,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: atman_runtime::provider::CallTiming::default(),
            model: String::new(),
            response_id: None,
        };
        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
            Box::pin(async move {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                Ok(msg)
            });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

const AGENT_CONTEXT_SESSION: &str = r#"
flow agent(user_prompt: string) -> string {
    _prompt_lands_via_begin_turn = user_prompt
    return subflow(agent_loop, 0)
}

flow agent_loop(iteration: int) -> string {
    when iteration >= 5 {
        return "[agent: max iterations]"
    }
    reply = llm.call(
        model: "recording",
        context: "session",
        tools: ["fs.read", "session.push"],
    )
    tool_uses = extract_tool_uses(reply)
    when is_empty(tool_uses) {
        return text_concat(reply)
    }
    tool_results = dispatch_all(tool_uses)
    session.push(tool_results)
    j = iteration + 1
    return subflow(agent_loop, j)
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn context_session_feeds_session_history_into_llm_call() {
    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("recording", "recording", 200_000, None),
        common::model_for_provider("recording-full-window", "recording", 200_000, None),
    ]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("data.txt");
    tokio::fs::write(&file_path, "hello from file")
        .await
        .unwrap();

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            MessagePart::Text {
                text: "checking".into(),
            },
            MessagePart::ToolUse {
                id: "call_0".into(),
                name: "fs.read".into(),
                input: serde_json::json!({"path": file_path.display().to_string()}),
            },
        ],
        vec![MessagePart::Text {
            text: "done: read the file".into(),
        }],
    ]));

    let session = std::sync::Arc::new(Session::open(dir.path()).unwrap());
    let session_id = session.id().to_string();
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);
    ex.providers.register(provider.clone());

    let file = parse_file(AGENT_CONTEXT_SESSION).unwrap();

    let turn_id = TurnId::now();
    let user_msg = Message::user_text(turn_id.clone(), "what's in the file?");
    session.begin_turn(user_msg);

    let result = ex
        .run_in_turn(
            &file,
            "agent",
            vec![(
                "user_prompt".into(),
                Value::Str("what's in the file?".into()),
            )],
            Some(turn_id),
            Some(session.clone()),
        )
        .await;
    let result = match result {
        Ok(v) => v,
        Err(e) => {
            let msgs = session.messages();
            eprintln!("error: {e}");
            eprintln!(
                "session messages: {:?}",
                msgs.iter()
                    .map(|m| (m.role, m.text_concat()))
                    .collect::<Vec<_>>()
            );
            panic!("agent flow failed: {e}");
        }
    };
    session.end_turn();

    match result {
        Value::Str(s) => assert!(s.contains("done: read the file"), "got: {s}"),
        other => panic!("expected str, got {other:?}"),
    }

    let captured = provider.captured();
    assert_eq!(captured.len(), 2, "two LLM calls expected");

    let first = &captured[0];
    assert!(
        first.iter().any(|m| {
            m.role == MessageRole::User && m.text_concat().contains("what's in the file?")
        }),
        "first call should include the user message from session, got: {:?}",
        first
            .iter()
            .map(|m| (m.role, m.text_concat()))
            .collect::<Vec<_>>()
    );

    let second = &captured[1];
    let has_assistant_with_tool_use = second.iter().any(|m| {
        m.role == MessageRole::Assistant
            && m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
    });
    let has_tool_result = second.iter().any(|m| {
        m.role == MessageRole::Tool
            && m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
    });
    assert!(
        has_assistant_with_tool_use,
        "second call should see the assistant message with tool_use (pushed via session.push)"
    );
    assert!(
        has_tool_result,
        "second call should see the tool_result (pushed via session.push)"
    );

    let final_session = session.messages();
    let durable_tool_results = final_session
        .iter()
        .filter(|m| {
            m.role == MessageRole::Tool
                && m.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::ToolResult { content, .. }
                            if content.contains("hello from file")
                    )
                })
        })
        .count();
    assert_eq!(
        durable_tool_results, 1,
        "explicit session.push should persist exactly one tool result"
    );
    assert!(
        final_session
            .iter()
            .all(|m| !m.text_concat().contains("done: read the file")),
        "automatic assistant output must not enter durable session history"
    );

    session.shutdown().await;
    let reopened = Session::open_existing(dir.path(), &session_id).unwrap();
    let reopened_messages = reopened.messages();
    assert_eq!(
        reopened_messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolResult { content, .. }
                                if content.contains("hello from file")
                        )
                    })
            })
            .count(),
        1,
        "reopen must preserve only the explicitly pushed tool result"
    );
    assert!(
        reopened_messages
            .iter()
            .all(|m| !m.text_concat().contains("done: read the file")),
        "reopen must not promote automatic assistant output into durable history"
    );
    reopened.shutdown().await;
}

const SINGLE_SESSION_CALL: &str = r#"
flow one_shot() -> string {
    reply = llm.call(model: "recording-full-window", context: "session")
    return text_concat(reply)
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn reopened_session_context_only_restores_explicit_durable_messages() {
    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("recording", "recording", 200_000, None),
        common::model_for_provider("recording-full-window", "recording", 200_000, None),
    ]))
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let session = Session::open(tmp.path()).unwrap();
        let root = FlowRunId::now();
        let spawned = FlowRunId::now();
        session.sink().emit(Event::FlowStart {
            run_id: root.clone(),
            flow_name: "root".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        });
        session.sink().emit(Event::FlowStart {
            run_id: spawned.clone(),
            flow_name: "worker".into(),
            parent_run_id: Some(root.clone()),
            parent_node_id: None,
            spawned: true,
        });
        session.append_message(
            Message::user_text(TurnId::now(), "ambiguous execution-owned root message"),
            Some(root.clone()),
        );
        session.append_message(
            Message::user_text(TurnId::now(), "isolated spawned child message"),
            Some(spawned.clone()),
        );
        session.append_message(
            Message::user_text(TurnId::now(), "explicit durable root message"),
            None,
        );
        let sid = session.id().to_string();
        session.shutdown().await;
        sid
    };
    let session = Arc::new(Session::open_existing(tmp.path(), &sid).unwrap());
    let provider = Arc::new(RecordingProvider::new(vec![vec![MessagePart::Text {
        text: "ok".into(),
    }]]));
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);
    ex.providers.register(provider.clone());
    let file = parse_file(SINGLE_SESSION_CALL).unwrap();

    ex.run_in_turn(
        &file,
        "one_shot",
        vec![],
        Some(TurnId::now()),
        Some(session.clone()),
    )
    .await
    .unwrap();

    let captured = provider.captured();
    assert_eq!(captured.len(), 1);
    let texts = captured[0]
        .iter()
        .map(Message::text_concat)
        .collect::<Vec<_>>();
    assert!(
        texts
            .iter()
            .any(|text| text == "explicit durable root message")
    );
    assert!(
        !texts
            .iter()
            .any(|text| text == "ambiguous execution-owned root message")
    );
    assert!(
        !texts
            .iter()
            .any(|text| text == "isolated spawned child message")
    );
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn context_session_sends_the_full_live_window_without_request_projection() {
    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("recording", "recording", 200_000, None),
        common::model_for_provider("recording-full-window", "recording", 200_000, None),
    ]))
    .await;
    let provider = Arc::new(RecordingProvider::new(vec![vec![MessagePart::Text {
        text: "ok".into(),
    }]]));
    let session = Arc::new(Session::open_ephemeral());
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);
    ex.providers.register(provider.clone());
    let file = parse_file(SINGLE_SESSION_CALL).unwrap();

    for i in 0..7 {
        session.append_message(
            Message::user_text(TurnId::now(), format!("session turn {i}")),
            None,
        );
    }
    let turn_id = TurnId::now();
    ex.run_in_turn(
        &file,
        "one_shot",
        vec![],
        Some(turn_id),
        Some(session.clone()),
    )
    .await
    .unwrap();

    let captured = provider.captured();
    assert_eq!(captured.len(), 1);
    let user_texts = captured[0]
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .map(Message::text_concat)
        .collect::<Vec<_>>();
    assert_eq!(
        user_texts.len(),
        7,
        "context:session must use the complete live message window"
    );
    assert_eq!(
        user_texts.first().map(String::as_str),
        Some("session turn 0")
    );
    assert_eq!(
        user_texts.last().map(String::as_str),
        Some("session turn 6")
    );
}

const AGENT_CONTEXT_NONE: &str = r#"
flow one_shot(user_prompt: string) -> string {
    reply = llm.call(
        model: "recording",
        prompt: user_prompt,
    )
    return text_concat(reply)
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn context_none_default_does_not_read_session_history() {
    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("recording", "recording", 200_000, None),
        common::model_for_provider("recording-full-window", "recording", 200_000, None),
    ]))
    .await;
    let provider = Arc::new(RecordingProvider::new(vec![vec![MessagePart::Text {
        text: "ok".into(),
    }]]));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);
    ex.providers.register(provider.clone());

    let file = parse_file(AGENT_CONTEXT_NONE).unwrap();

    let turn_id = TurnId::now();
    session.begin_turn(Message::user_text(
        turn_id.clone(),
        "pre-existing session msg",
    ));

    let result = ex
        .run_in_turn(
            &file,
            "one_shot",
            vec![("user_prompt".into(), Value::Str("just this prompt".into()))],
            Some(turn_id),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();

    match result {
        Value::Str(s) => assert!(s.contains("ok"), "got: {s}"),
        other => panic!("expected str, got {other:?}"),
    }

    let captured = provider.captured();
    assert_eq!(captured.len(), 1);
    let msgs = &captured[0];
    assert_eq!(msgs.len(), 1, "context:none should send exactly 1 message");
    assert_eq!(msgs[0].role, MessageRole::User);
    assert!(
        msgs[0].text_concat().contains("just this prompt"),
        "should only contain the prompt, got: {}",
        msgs[0].text_concat()
    );
    assert!(
        !msgs[0].text_concat().contains("pre-existing session msg"),
        "session history must NOT leak into context:none calls"
    );
}
