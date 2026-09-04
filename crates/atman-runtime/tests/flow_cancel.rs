mod common;

use std::sync::Arc;
use std::sync::Mutex;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Observable, TurnId};
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage, wrap_call_as_streaming,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RootInvocation, RuntimeError, Value};
use tokio_util::sync::CancellationToken;

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

#[tokio::test]
async fn flow_cancel_before_start_returns_cancelled_error() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = common::executor();
    common::register_provider(
        &executor,
        MockProvider::new("mock").with_model("mock", Value::Str("would-run".into())),
    );

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.cancel_flow();

    let err = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Cancelled(msg) if msg.contains("cancelled")));
}

#[tokio::test]
async fn root_invocation_cancel_token_is_independent_from_session_slot() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let file = parse_file(
        r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi", context: "session")
}
flow watched() -> string {
    reply = llm.call(model: "mock", prompt: "hi", context: "session")
    watch reply {
        on token(match: "unused") { warn("unused") }
    }
    return reply
}
"#,
    )
    .unwrap();
    let executor = common::executor();
    common::register_provider(
        &executor,
        MockProvider::new("mock").with_model("mock", Value::Str("would-run".into())),
    );

    let cancelled_invocation_session = Arc::new(Session::open_ephemeral());
    let cancelled_turn = TurnId::now();
    cancelled_invocation_session.begin_turn(user_msg(cancelled_turn.clone(), "cancel"));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = executor
        .run_with_invocation(
            &file,
            "ask",
            vec![],
            RootInvocation {
                turn_id: Some(cancelled_turn),
                session: Some(cancelled_invocation_session),
                flow_cancel: Some(cancelled),
                ..RootInvocation::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Cancelled(_)));

    executor.tool_ctx.flow_cancel.cancel();
    for flow in ["ask", "watched"] {
        let cancelled_session_slot = Arc::new(Session::open_ephemeral());
        let live_turn = TurnId::now();
        cancelled_session_slot.begin_turn(user_msg(live_turn.clone(), "continue"));
        cancelled_session_slot.cancel_flow();
        let output = executor
            .run_with_invocation(
                &file,
                flow,
                vec![],
                RootInvocation {
                    turn_id: Some(live_turn),
                    session: Some(cancelled_session_slot),
                    flow_cancel: Some(CancellationToken::new()),
                    ..RootInvocation::default()
                },
            )
            .await
            .unwrap();
        assert!(
            !output.is_err(),
            "{flow}: explicit live token must keep the run active"
        );
    }
}

struct CancelAfterFirstProvider {
    name: String,
    calls: Arc<Mutex<usize>>,
    cancel: Arc<dyn Fn() + Send + Sync>,
}

impl Provider for CancelAfterFirstProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let cancel = self.cancel.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            let idx = {
                let mut c = calls.lock().unwrap();
                *c += 1;
                *c
            };
            if idx == 1 {
                cancel();
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
        let cancel = self.cancel.clone();
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
                cancel();
            }
            Ok(AssistantMessage::text_only(Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: format!("call-{idx}"),
                }],
                turn_id,
                origin: MessageOrigin::User,
            }))
        }))
    }
}

#[tokio::test]
async fn flow_cancel_between_nodes_stops_before_next_node_runs() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "prov", "prov", 8_192, None,
        )]))
        .await;
    let src = r#"flow chained() -> string {
    a = llm.call(model: "prov", prompt: "first")
    b = llm.call(model: "prov", prompt: "second")
    return b
}
"#;
    let file = parse_file(src).unwrap();

    for through_entry in [false, true] {
        let session = Arc::new(Session::open_ephemeral());
        let turn_id = TurnId::now();
        session.begin_turn(user_msg(turn_id.clone(), "go"));

        let calls = Arc::new(Mutex::new(0usize));
        let cancel_session = session.clone();
        let executor = Executor::new();
        executor
            .providers
            .register(Arc::new(CancelAfterFirstProvider {
                name: "prov".into(),
                calls: calls.clone(),
                cancel: Arc::new(move || {
                    if through_entry {
                        cancel_session
                            .flow_registry
                            .lookup("root")
                            .unwrap()
                            .cancel
                            .cancel();
                    } else {
                        cancel_session.cancel_flow();
                    }
                }),
            }));

        let out = executor
            .run_with_invocation(
                &file,
                "chained",
                vec![],
                RootInvocation {
                    turn_id: Some(turn_id),
                    session: Some(session.clone()),
                    flow_cancel: through_entry.then(CancellationToken::new),
                    ..RootInvocation::default()
                },
            )
            .await;
        assert!(out.is_err(), "flow should abort after cancel_flow");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "second llm call must be skipped by cancel-poll at eval_node entry"
        );
        assert!(matches!(
            *session
                .flow_registry
                .lookup("root")
                .unwrap()
                .status
                .lock()
                .unwrap(),
            atman_runtime::tools::agent_ctrl::FlowRunStatus::Killed { .. }
        ));
    }
}
