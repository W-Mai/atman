mod common;

use std::sync::Arc;

use atman_runtime::event::EventSink;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session_meta::{NameSource, SessionMeta};
use atman_runtime::{Executor, Session};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

struct ThinkingProvider(MockProvider);

fn with_thinking(
    mut reply: atman_runtime::provider::AssistantMessage,
) -> atman_runtime::provider::AssistantMessage {
    reply.message.parts.insert(
        0,
        atman_runtime::message::MessagePart::Thinking {
            thinking: "Choose a concise title".into(),
            signature: None,
        },
    );
    reply
}

impl atman_runtime::provider::Provider for ThinkingProvider {
    fn name(&self) -> &str {
        "cheap"
    }

    fn call<'a>(
        &'a self,
        req: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::tool::BoxFut<
        'a,
        Result<atman_runtime::provider::AssistantMessage, atman_runtime::RuntimeError>,
    > {
        Box::pin(async move { self.0.call(req).await.map(with_thinking) })
    }

    fn call_streaming(
        &self,
        req: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::event::Observable<atman_runtime::provider::AssistantMessage> {
        let stream = self.0.call_streaming(req);
        atman_runtime::event::Observable {
            output: Box::pin(async move { stream.output.await.map(with_thinking) }),
            events: stream.events,
            cancel: stream.cancel,
        }
    }
}

#[tokio::test]
async fn naming_returns_text_independently_of_reply_structure() {
    let _registry = common::ModelRegistryGuard::mock("cheap").await;
    for title in ["Fix flow naming", "2026", "true"] {
        let executor = Executor::with_events(EventSink::new());
        executor.providers.register(Arc::new(ThinkingProvider(
            MockProvider::new("cheap").with_fallback(Value::Str(title.into())),
        )));
        let result = atman_runtime::session_naming::suggest_session_title(&executor, None, &[])
            .await
            .unwrap();
        assert_eq!(result, title);
        assert!(executor.events.snapshot().is_empty());
    }
}

#[tokio::test]
async fn built_in_flow_generates_and_persists_session_name() {
    let _registry = common::ModelRegistryGuard::mock("cheap").await;
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(tmp.path()).unwrap());
    let executor = Executor::with_events(EventSink::new());
    executor.providers.register(Arc::new(
        MockProvider::new("cheap").with_fallback(Value::Str("Fix session switching".into())),
    ));

    let written = atman_runtime::session_naming::maybe_generate_session_name(&executor, &session)
        .await
        .unwrap();

    assert!(written);
    let meta = SessionMeta::load(session.dir()).unwrap();
    assert_eq!(meta.title.as_deref(), Some("Fix session switching"));
    assert_eq!(meta.name_source, NameSource::Auto);
    assert!(executor.events.snapshot().is_empty());
}

#[tokio::test]
async fn scheduled_auto_name_does_not_replace_user_name() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(tmp.path()).unwrap());
    SessionMeta::set_title(session.dir(), Some("Pinned by user".into())).unwrap();
    let executor = Executor::with_events(EventSink::new());

    let written = atman_runtime::session_naming::maybe_generate_session_name(&executor, &session)
        .await
        .unwrap();

    assert!(!written);
    let meta = SessionMeta::load(session.dir()).unwrap();
    assert_eq!(meta.title.as_deref(), Some("Pinned by user"));
    assert_eq!(meta.name_source, NameSource::User);
}

#[tokio::test]
async fn user_triggered_auto_name_replaces_user_name_and_source() {
    let _registry = common::ModelRegistryGuard::mock("cheap").await;
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(tmp.path()).unwrap());
    SessionMeta::set_title(session.dir(), Some("Pinned by user".into())).unwrap();
    let executor = Executor::with_events(EventSink::new());
    executor.providers.register(Arc::new(
        MockProvider::new("cheap").with_fallback(Value::Str("Replacement".into())),
    ));

    let written = atman_runtime::session_naming::force_generate_session_name(&executor, &session)
        .await
        .unwrap();

    assert!(written);
    let meta = SessionMeta::load(session.dir()).unwrap();
    assert_eq!(meta.title.as_deref(), Some("Replacement"));
    assert_eq!(meta.name_source, NameSource::Auto);
    assert_eq!(session.successful_flow_count(), 0);
}
