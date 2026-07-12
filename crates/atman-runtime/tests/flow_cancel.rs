use std::sync::Arc;
use std::sync::Mutex;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Observable, TurnId};
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage, wrap_call_as_streaming,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RuntimeError, Value};

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
    }
}

#[tokio::test]
async fn flow_cancel_before_start_returns_cancelled_error() {
    let src = r#"flow ask() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let mut executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("would-run".into())),
    ));

    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.cancel_flow();

    let err = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Cancelled(msg) if msg.contains("cancelled")));
}

struct CancelAfterFirstProvider {
    name: String,
    calls: Arc<Mutex<usize>>,
    session: Arc<Session>,
}

impl Provider for CancelAfterFirstProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let session = self.session.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            let idx = {
                let mut c = calls.lock().unwrap();
                *c += 1;
                *c
            };
            if idx == 1 {
                session.cancel_flow();
            }
            let turn_id = req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now);
            Ok(AssistantMessage {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text {
                        text: format!("call-{idx}"),
                    }],
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
        let session = self.session.clone();
        let calls = self.calls.clone();
        let turn_id = req
            .messages
            .first()
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        wrap_call_as_streaming(Box::pin(async move {
            let idx = {
                let mut c = calls.lock().unwrap();
                *c += 1;
                *c
            };
            if idx == 1 {
                session.cancel_flow();
            }
            Ok(AssistantMessage::text_only(Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: format!("call-{idx}"),
                }],
                turn_id,
            }))
        }))
    }
}

#[tokio::test]
async fn flow_cancel_between_nodes_stops_before_next_node_runs() {
    let src = r#"flow chained() -> string {
    a = llm { model: "prov", prompt: "first" }
    b = llm { model: "prov", prompt: "second" }
    return b
}
"#;
    let file = parse_file(src).unwrap();

    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "go"));

    let calls = Arc::new(Mutex::new(0usize));
    let mut executor = Executor::new();
    executor
        .providers
        .register(Arc::new(CancelAfterFirstProvider {
            name: "prov".into(),
            calls: calls.clone(),
            session: session.clone(),
        }));

    let out = executor
        .run_in_turn(&file, "chained", vec![], Some(turn_id), Some(&session))
        .await;
    assert!(out.is_err(), "flow should abort after cancel_flow");
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "second llm call must be skipped by cancel-poll at eval_node entry"
    );
}
