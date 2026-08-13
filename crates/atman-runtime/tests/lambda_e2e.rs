use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
use atman_runtime::model_registry::{MODEL_CONFIG_LOCK, ModelConfig, ModelEntry, set_model_config};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

fn register_model() {
    let _lock = MODEL_CONFIG_LOCK.lock().unwrap();
    set_model_config(ModelConfig {
        models: [(
            "m".into(),
            ModelEntry {
                model: "m".into(),
                context_budget: Some(8_192),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
}

fn run(src: &str, provider: MockProvider) -> Value {
    register_model();
    let parsed = parse_file(src).expect("parse");
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(Arc::new(provider));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(ex.run(&parsed, "test", vec![]))
        .expect("flow failed")
}

fn run_str(src: &str, provider: MockProvider) -> String {
    match run(src, provider) {
        Value::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn map_with_llm_classify() {
    let src = r#"
flow test() -> string {
    files = ["test_a.rs", "main.rs", "test_b.rs"]
    is_tests = list.map(files, |f| llm.classify(model: "m", prompt: "Is " + f + " a test file?"))
    return to_json_string(is_tests)
}
"#;
    // Mock returns "yes" for all — classify returns Bool(true)
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    let result = run_str(src, provider);
    assert!(result.contains("true"), "should have true values: {result}");
}

#[test]
fn filter_with_llm_classify() {
    let src = r#"
flow test() -> string {
    files = ["safe.txt", "danger.exe", "ok.rs"]
    safe = list.filter(files, |f| llm.classify(model: "m", prompt: "Is " + f + " safe?"))
    return to_json_string(safe)
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    let result = run_str(src, provider);
    // All pass filter since mock says "yes" → true for all
    assert!(
        result.contains("safe.txt"),
        "should keep safe.txt: {result}"
    );
    assert!(
        result.contains("danger.exe"),
        "mock says yes to all: {result}"
    );
}

#[test]
fn map_with_llm_extract() {
    let src = r#"
flow test() -> string {
    reviews = list.map(["file1", "file2"], |f|
        llm.extract(
            model: "m",
            prompt: "Review " + f,
            fields: {
                valid: bool -- "is valid",
                score: int -- "score 0-100",
            },
        )
    )
    first = list.find(reviews, |r| r.valid == true)
    return to_json_string(first.valid) + "|" + to_json_string(first.score)
}
"#;
    let json = r#"{"valid": true, "score": 85}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    let result = run_str(src, provider);
    assert_eq!(result, "true|85");
}

#[test]
fn generate_branches_then_fanout() {
    let src = r#"
flow test() -> string {
    tasks = llm.generate_branches(model: "m", prompt: "break down", count: 3)
    results = fanout tasks { |t| t + "_done" } collect: all
    return to_json_string(results)
}
"#;
    let json = r#"["task1", "task2", "task3"]"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    let result = run_str(src, provider);
    assert!(result.contains("task1_done"), "{result}");
    assert!(result.contains("task2_done"), "{result}");
    assert!(result.contains("task3_done"), "{result}");
}

#[test]
fn chain_branches_fanout_map() {
    let src = r#"
flow test() -> string {
    tasks = ["a", "b"]
    processed = list.map(
        fanout tasks { |t| t + "!" } collect: all,
        |r| r + "?"
    )
    return to_json_string(processed)
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("ok".into()));
    let result = run_str(src, provider);
    assert!(result.contains("a!?"), "{result}");
    assert!(result.contains("b!?"), "{result}");
}

#[test]
fn map_classify_then_all() {
    let src = r#"
flow test() -> string {
    results = list.map(["item1", "item2"], |item|
        llm.classify(model: "m", prompt: "Is " + item + " good?")
    )
    all_good = list.all(results, |r| r == true)
    return to_json_string(all_good)
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    let result = run_str(src, provider);
    assert_eq!(result, "true");
}
