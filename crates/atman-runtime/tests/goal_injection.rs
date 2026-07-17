use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::goal::GoalStore;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::LlmRequest;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Session, Value};

fn mock_that_echoes_system() -> Arc<CapturedProvider> {
    Arc::new(CapturedProvider::default())
}

#[derive(Default)]
struct CapturedProvider {
    last_system: std::sync::Mutex<Option<String>>,
    last_messages: std::sync::Mutex<Vec<Message>>,
}

impl CapturedProvider {
    fn last_system(&self) -> Option<String> {
        self.last_system.lock().unwrap().clone()
    }
    fn last_messages(&self) -> Vec<Message> {
        self.last_messages.lock().unwrap().clone()
    }
}

impl atman_runtime::provider::Provider for CapturedProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn call<'a>(
        &'a self,
        req: LlmRequest,
    ) -> atman_runtime::tool::BoxFut<
        'a,
        Result<atman_runtime::provider::AssistantMessage, atman_runtime::error::RuntimeError>,
    > {
        Box::pin(async move {
            *self.last_system.lock().unwrap() = req.system.clone();
            *self.last_messages.lock().unwrap() = req.messages.clone();
            let turn_id = atman_runtime::event::TurnId::now();
            Ok(atman_runtime::provider::AssistantMessage::text_only(
                Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text { text: "ok".into() }],
                    turn_id,
                },
            ))
        })
    }
    fn call_streaming(
        &self,
        req: LlmRequest,
    ) -> atman_runtime::event::Observable<atman_runtime::provider::AssistantMessage> {
        let f: atman_runtime::tool::BoxFut<
            'static,
            Result<atman_runtime::provider::AssistantMessage, atman_runtime::error::RuntimeError>,
        > = {
            let key = self.name().to_string();
            let _ = key;
            let text = "ok".to_string();
            Box::pin(async move {
                let turn_id = atman_runtime::event::TurnId::now();
                Ok(atman_runtime::provider::AssistantMessage::text_only(
                    Message {
                        role: MessageRole::Assistant,
                        parts: vec![MessagePart::Text { text }],
                        turn_id,
                    },
                ))
            })
        };
        *self.last_system.lock().unwrap() = req.system.clone();
        *self.last_messages.lock().unwrap() = req.messages.clone();
        atman_runtime::provider::wrap_call_as_streaming(f)
    }
}

#[tokio::test]
async fn goal_prefix_lands_in_llm_system_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    GoalStore::at(session.dir())
        .set("ship the atman agent")
        .unwrap();

    let provider = mock_that_echoes_system();
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());

    let src = r#"flow t() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    let _ = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    let msgs = provider.last_messages();
    let goal_msg: String = msgs
        .iter()
        .map(|m| m.text_concat())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        goal_msg.contains("ship the atman agent"),
        "want goal text in messages, got: {goal_msg}"
    );
    assert!(
        goal_msg.contains("[session goal]") && goal_msg.contains("[/session goal]"),
        "want goal delimiters, got: {goal_msg}"
    );
}

#[tokio::test]
async fn goal_prefix_prepends_user_system_and_keeps_both() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    GoalStore::at(session.dir()).set("stay minimal").unwrap();

    let provider = mock_that_echoes_system();
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());

    let src = r#"flow t() -> string {
    return llm {
        model: "mock"
        prompt: "hi"
        system: "you are a helpful assistant"
    }
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    let _ = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    let seen = provider.last_system().unwrap();
    assert!(
        seen.contains("you are a helpful assistant"),
        "user system must stay in system prompt: {seen}"
    );
    let msgs = provider.last_messages();
    let goal_msg: String = msgs
        .iter()
        .map(|m| m.text_concat())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        goal_msg.contains("stay minimal"),
        "goal must be in messages: {goal_msg}"
    );
}

#[tokio::test]
async fn no_goal_leaves_system_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    let provider = mock_that_echoes_system();
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());

    let src = r#"flow t() -> string {
    return llm { model: "mock", prompt: "hi", system: "only-user" }
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert_eq!(provider.last_system().as_deref(), Some("only-user"));
}

#[tokio::test]
async fn goal_survives_multiple_turns_in_same_session() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    GoalStore::at(session.dir()).set("persistent goal").unwrap();

    let provider = mock_that_echoes_system();
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());

    let src = r#"flow t() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();

    for _ in 0..3 {
        let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
        session.begin_turn(user_msg);
        ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
            .await
            .unwrap();
        session.end_turn();
        let msgs = provider.last_messages();
        let joined: String = msgs
            .iter()
            .map(|m| m.text_concat())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("persistent goal"),
            "turn missed goal: {joined}"
        );
    }
}

#[tokio::test]
async fn dsl_goal_set_persists_to_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    let plan = Arc::new(atman_runtime::memory::PlanStore::at(session.dir()));
    atman_runtime::tools::register_memory(&mut ex.tools, todo, conf, goal.clone(), plan);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok".into())),
    ));

    let src = r#"flow t() -> string {
    memory.goal.set(text: "via dsl")
    return "done"
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert_eq!(goal.get().unwrap(), "via dsl");
}
