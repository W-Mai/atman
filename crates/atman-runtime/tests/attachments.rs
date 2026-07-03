use std::sync::{Arc, Mutex};

use atman_dsl::parse::parse_file;
use atman_runtime::event::Observable;
use atman_runtime::provider::{Attachment, LlmRequest, Provider, wrap_call_as_streaming};
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RuntimeError, Value};

struct RecorderProvider {
    inner_name: String,
    recorded: Arc<Mutex<Vec<Vec<Attachment>>>>,
}

impl Provider for RecorderProvider {
    fn name(&self) -> &str {
        &self.inner_name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>> {
        let recorded = self.recorded.clone();
        Box::pin(async move {
            recorded.lock().unwrap().push(req.attachments.clone());
            Ok(Value::Str("ok".into()))
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<Value> {
        let recorded = self.recorded.clone();
        let attachments = req.attachments.clone();
        wrap_call_as_streaming(Box::pin(async move {
            recorded.lock().unwrap().push(attachments);
            Ok(Value::Str("ok".into()))
        }))
    }
}

#[tokio::test]
async fn pending_attachments_drain_into_next_llm_call() {
    let src = r#"flow ask() -> string {
    return llm { model: "claude-x", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let recorded: Arc<Mutex<Vec<Vec<Attachment>>>> = Arc::new(Mutex::new(Vec::new()));

    let mut executor = Executor::new();
    executor.providers.register(Arc::new(RecorderProvider {
        inner_name: "claude-x".into(),
        recorded: recorded.clone(),
    }));

    executor.push_attachment(Attachment::image("/tmp/pic.png"));
    assert_eq!(executor.pending_attachment_count(), 1);

    let v = executor.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(v, Value::Str(s) if s == "ok"));

    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 1);
    assert_eq!(calls[0][0].path.to_string_lossy(), "/tmp/pic.png");

    assert_eq!(executor.pending_attachment_count(), 0);
}

#[tokio::test]
async fn no_pending_yields_empty_attachments() {
    let src = r#"flow ask() -> string {
    return llm { model: "claude-x", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let recorded: Arc<Mutex<Vec<Vec<Attachment>>>> = Arc::new(Mutex::new(Vec::new()));

    let mut executor = Executor::new();
    executor.providers.register(Arc::new(RecorderProvider {
        inner_name: "claude-x".into(),
        recorded: recorded.clone(),
    }));

    executor.run(&file, "ask", vec![]).await.unwrap();
    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].is_empty());
}
