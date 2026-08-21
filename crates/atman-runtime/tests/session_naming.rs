mod common;

use std::sync::Arc;

use atman_runtime::event::EventSink;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session_meta::{NameSource, SessionMeta};
use atman_runtime::{Executor, Session, Value};

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
