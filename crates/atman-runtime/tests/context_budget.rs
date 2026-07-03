use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::eval::truncate_prompt_to_budget;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value};

#[test]
fn truncate_leaves_short_prompt_unchanged() {
    let out = truncate_prompt_to_budget("hello".into(), 100);
    assert_eq!(out, "hello");
}

#[test]
fn truncate_keeps_head_and_tail_dropping_middle() {
    let long: String = "A".repeat(400) + &"B".repeat(400) + &"C".repeat(400);
    let out = truncate_prompt_to_budget(long, 100);
    assert!(out.starts_with("AAAA"));
    assert!(out.ends_with("CCCC"));
    assert!(out.contains("truncated"));
    assert!(!out.contains("BBBBBBBBBB"));
    assert!(out.len() < 500);
}

#[tokio::test]
async fn context_budget_kwarg_shrinks_prompt_before_provider() {
    let src = r#"flow t(text: string) -> string {
    return llm {
        model: "echo"
        prompt: text
        context_budget: 30
    }
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("echo").with_fallback(Value::Str("ok".into())),
    ));

    let long_prompt: String = "X".repeat(2000);
    let out = ex
        .run(&file, "t", vec![("text".into(), Value::Str(long_prompt))])
        .await
        .unwrap();
    assert!(matches!(out, Value::Str(s) if s == "ok"));
}
