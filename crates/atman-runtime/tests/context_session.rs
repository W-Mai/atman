use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable, TurnId};
use atman_runtime::message::{Message, MessagePart, MessageRole};
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
    reply = llm {
        model: "recording"
        context: session
        tools: [fs.read, session.push]
    }
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

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let mut ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut ex.tools);
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
    assert!(
        final_session.iter().any(|m| {
            m.role == MessageRole::Tool
                && m.parts.iter().any(|p| {
                    if let MessagePart::ToolResult { content, .. } = p {
                        content.contains("hello from file")
                    } else {
                        false
                    }
                })
        }),
        "session should contain the tool_result with file content"
    );
}

const AGENT_CONTEXT_NONE: &str = r#"
flow one_shot(user_prompt: string) -> string {
    reply = llm {
        model: "recording"
        prompt: user_prompt
    }
    return text_concat(reply)
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn context_none_default_does_not_read_session_history() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![MessagePart::Text {
        text: "ok".into(),
    }]]));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let mut ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut ex.tools);
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
