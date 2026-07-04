use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable, TurnId};
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage};
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Value, tools};

struct ScriptedAgentProvider {
    turns: Vec<AgentTurn>,
    calls: AtomicUsize,
}

enum AgentTurn {
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    FinalText(String),
}

impl ScriptedAgentProvider {
    fn new(turns: Vec<AgentTurn>) -> Self {
        Self {
            turns,
            calls: AtomicUsize::new(0),
        }
    }

    fn make_message(&self, turn_id: TurnId, idx: usize) -> AssistantMessage {
        let parts = match self.turns.get(idx) {
            Some(AgentTurn::ToolUse { name, input }) => vec![
                MessagePart::Text {
                    text: "let me check".into(),
                },
                MessagePart::ToolUse {
                    id: format!("call_{idx}"),
                    name: name.clone(),
                    input: input.clone(),
                },
            ],
            Some(AgentTurn::FinalText(t)) => vec![MessagePart::Text { text: t.clone() }],
            None => vec![MessagePart::Text {
                text: "[scripted: exhausted]".into(),
            }],
        };
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
        }
    }
}

impl Provider for ScriptedAgentProvider {
    fn name(&self) -> &str {
        "scripted-agent"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let turn_id = req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now);
            Ok(self.make_message(turn_id, idx))
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;
        let (tx, events) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let turn_id = req
            .messages
            .first()
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let msg = self.make_message(turn_id, idx);
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

fn agent_source() -> &'static str {
    r#"
flow agent(user_prompt: string) -> string {
    initial = user_msg(user_prompt)
    return subflow(agent_loop, [initial], 0)
}

flow agent_loop(messages: list, iteration: int) -> string {
    when iteration >= 5 {
        return "[agent: max iterations reached]"
    }
    reply = llm {
        model: "scripted"
        messages: messages
        tools: [fs.read]
    }
    tool_uses = extract_tool_uses(reply)
    when is_empty(tool_uses) {
        return text_concat(reply)
    }
    tool_results = dispatch_all(tool_uses)
    new_history = concat(messages, concat([reply], tool_results))
    next_iter = iteration + 1
    return subflow(agent_loop, new_history, next_iter)
}
"#
}

#[tokio::test(flavor = "current_thread")]
async fn agent_flow_dispatches_tool_use_and_returns_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let readme = dir.path().join("hello.txt");
    tokio::fs::write(&readme, "world of atman").await.unwrap();

    let provider = Arc::new(ScriptedAgentProvider::new(vec![
        AgentTurn::ToolUse {
            name: "fs.read".into(),
            input: serde_json::json!({"path": readme.display().to_string()}),
        },
        AgentTurn::FinalText("summary: file said hello".into()),
    ]));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());
    let file = parse_file(agent_source()).unwrap();
    let result = ex
        .run(
            &file,
            "agent",
            vec![(
                "user_prompt".into(),
                Value::Str("what's in the file?".into()),
            )],
        )
        .await
        .unwrap();
    match result {
        Value::Str(s) => assert!(s.contains("summary: file said hello"), "got: {s}"),
        other => panic!("expected str, got {other:?}"),
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "two llm turns expected"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn agent_flow_hits_max_iterations_when_llm_keeps_calling_tools() {
    let provider = Arc::new(ScriptedAgentProvider::new(
        (0..10)
            .map(|i| AgentTurn::ToolUse {
                name: "fs.read".into(),
                input: serde_json::json!({"path": format!("no-such-{i}.txt")}),
            })
            .collect(),
    ));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());
    let file = parse_file(agent_source()).unwrap();
    let result = ex
        .run(
            &file,
            "agent",
            vec![("user_prompt".into(), Value::Str("loop forever".into()))],
        )
        .await
        .unwrap();
    match result {
        Value::Str(s) => assert!(
            s.contains("max iterations reached"),
            "expected iteration cap, got: {s}"
        ),
        other => panic!("expected str, got {other:?}"),
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        5,
        "should stop at the iteration cap"
    );
}
