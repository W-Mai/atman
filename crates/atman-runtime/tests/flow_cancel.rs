mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use atman_rt::parse_file;
use atman_runtime::event::Observable;
use atman_runtime::event::{Event, FlowStatus};
type TurnId = atman_rt::TurnId<atman_runtime::event::AtmanUuid>;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage, wrap_call_as_streaming,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::stream::StreamFrame;
use atman_runtime::tool::BoxFut;
use atman_runtime::tool::{Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use atman_runtime::{Executor, RuntimeError};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

struct ApprovalProbeTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for ApprovalProbeTool {
    fn name(&self) -> &str {
        "test.approval_probe"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn requires_call_intent(&self) -> bool {
        false
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Int(1)) })
    }
}

#[tokio::test]
async fn cancelling_pending_tool_approval_finishes_the_same_invocation_once() {
    let file = parse_file(
        r#"flow guarded() -> int {
    return test.approval_probe()
}
"#,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let executor = Executor::new();
    executor.tools.register(Arc::new(ApprovalProbeTool {
        calls: Arc::clone(&calls),
    }));
    let events = executor.events.clone();
    let session = Arc::new(Session::open_ephemeral());
    let _permission_client = session.permission_broker().register_client();
    let mut frames = session.stream_subscribe();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "run guarded tool"));

    let run_session = Arc::clone(&session);
    let run = tokio::spawn(async move {
        executor
            .run_in_turn(&file, "guarded", vec![], Some(turn_id), Some(run_session))
            .await
    });

    let tool_use_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let StreamFrame::ToolUseStart { tool, id, .. } = frames.recv().await.unwrap()
                && tool == "test.approval_probe"
            {
                break id;
            }
        }
    })
    .await
    .expect("tool approval must become pending");
    session.cancel_flow();

    let result = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("cancelled approval must wake the flow")
        .expect("executor task must not panic");
    assert!(matches!(result, Err(RuntimeError::Cancelled(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut matching_done = 0;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match frames.recv().await.unwrap() {
                StreamFrame::ToolUseDone { tool, ok, id, .. }
                    if tool == "test.approval_probe" && id == tool_use_id =>
                {
                    assert!(!ok);
                    matching_done += 1;
                }
                StreamFrame::FlowDone { flow_name, .. } if flow_name == "guarded" => break,
                _ => {}
            }
        }
    })
    .await
    .expect("cancelled flow must publish terminal frames");
    assert_eq!(matching_done, 1);
    assert!(events.snapshot().iter().any(|event| matches!(
        event,
        Event::FlowEnd {
            flow_name,
            status: FlowStatus::Cancelled,
            ..
        } if flow_name == "guarded"
    )));
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

    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "go"));

    let calls = Arc::new(Mutex::new(0usize));
    let executor = Executor::new();
    executor
        .providers
        .register(Arc::new(CancelAfterFirstProvider {
            name: "prov".into(),
            calls: calls.clone(),
            session: session.clone(),
        }));

    let out = executor
        .run_in_turn(
            &file,
            "chained",
            vec![],
            Some(turn_id),
            Some(session.clone()),
        )
        .await;
    assert!(out.is_err(), "flow should abort after cancel_flow");
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "second llm call must be skipped by cancel-poll at eval_node entry"
    );
}

struct PendingProvider {
    started: Arc<tokio::sync::Notify>,
}

impl Provider for PendingProvider {
    fn name(&self) -> &str {
        "pending"
    }

    fn call<'a>(&'a self, _req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let started = self.started.clone();
        Box::pin(async move {
            started.notify_one();
            std::future::pending().await
        })
    }

    fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
        let started = self.started.clone();
        wrap_call_as_streaming(Box::pin(async move {
            started.notify_one();
            std::future::pending().await
        }))
    }
}

#[tokio::test]
async fn cancelling_awaited_flow_closes_child_before_parent() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "pending", "pending", 8_192, None,
        )]))
        .await;
    let file = parse_file(
        r#"flow child() -> string {
    return llm.call(model: "pending", prompt: "hold")
}

flow parent() -> string {
    return child().await
}
"#,
    )
    .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let executor = Executor::new();
    executor.providers.register(Arc::new(PendingProvider {
        started: started.clone(),
    }));
    let events = executor.events.clone();
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "go"));

    let run_session = session.clone();
    let run = tokio::spawn(async move {
        executor
            .run_in_turn(&file, "parent", vec![], Some(turn_id), Some(run_session))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
        .await
        .expect("child LLM call must start");
    session.cancel_flow();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), run)
        .await
        .expect("cancelled flow must finish")
        .expect("executor task must not panic");
    assert!(matches!(result, Err(RuntimeError::Cancelled(_))));

    let ends = events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            Event::FlowEnd {
                flow_name, status, ..
            } => Some((flow_name, status)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ends.iter()
            .map(|(name, status)| (name.as_str(), matches!(status, FlowStatus::Cancelled)))
            .collect::<Vec<_>>(),
        vec![("child", true), ("parent", true)]
    );
}

#[tokio::test]
async fn cancelling_fanout_closes_branch_before_parent() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "pending", "pending", 8_192, None,
        )]))
        .await;
    let file = parse_file(
        r#"flow child() -> string {
    return llm.call(model: "pending", prompt: "hold")
}

flow parent() -> [string] {
    return fanout [child()]
}
"#,
    )
    .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let executor = Executor::new();
    executor.providers.register(Arc::new(PendingProvider {
        started: started.clone(),
    }));
    let events = executor.events.clone();
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "go"));
    let run_session = session.clone();
    let run = tokio::spawn(async move {
        executor
            .run_in_turn(&file, "parent", vec![], Some(turn_id), Some(run_session))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
        .await
        .expect("fanout child must start");
    session.cancel_flow();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), run)
        .await
        .expect("cancelled fanout must finish")
        .expect("executor task must not panic");
    assert!(matches!(result, Err(RuntimeError::Cancelled(_))));

    let events = events.snapshot();
    let branch_start = events
        .iter()
        .filter(|event| matches!(event, Event::FlowNodeStart { label, .. } if label == "branch[0]"))
        .count();
    let branch_end = events
        .iter()
        .position(|event| matches!(event, Event::FlowNodeEnd { node_id, .. } if node_id.ends_with("branch[0]")))
        .expect("cancelled branch must have a terminal node event");
    let parent_end = events
        .iter()
        .position(
            |event| matches!(event, Event::FlowEnd { flow_name, .. } if flow_name == "parent"),
        )
        .expect("parent must have a terminal flow event");
    assert_eq!(branch_start, 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::FlowNodeEnd { node_id, .. } if node_id.ends_with("branch[0]")))
            .count(),
        1
    );
    assert!(branch_end < parent_end);
}
