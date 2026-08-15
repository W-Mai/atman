use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
mod common;

use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

fn run(src: &str, provider: MockProvider) -> Value {
    let _registry =
        common::SyncModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "m", "mock", 8_192, None,
        )]));
    let parsed = parse_file(src).expect("parse");
    let ex = Executor::new();
    ex.providers.register(Arc::new(provider));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(ex.run(&parsed, "test", vec![]))
        .expect("flow failed")
}

// === llm.classify ===

#[test]
fn classify_binary_yes() {
    let src = r#"
flow test() -> string {
    result = llm.classify(model: "m", prompt: "Is the sky blue?")
    when result {
        return "yes"
    }
    return "no"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "yes"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_binary_no() {
    let src = r#"
flow test() -> string {
    result = llm.classify(model: "m", prompt: "Is the sky green?")
    when result {
        return "yes"
    }
    return "no"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("no".into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "no"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_binary_bare_bool() {
    // LLM returns bare `true` → assistant_message_to_value → Value::Bool(true)
    let src = r#"
flow test() -> string {
    when llm.classify(model: "m", prompt: "safe?") {
        return "approved"
    }
    return "denied"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Bool(true));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "approved"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_binary_bare_int() {
    // LLM returns bare `1` → assistant_message_to_value → Value::Int(1)
    let src = r#"
flow test() -> string {
    when llm.classify(model: "m", prompt: "safe?") {
        return "approved"
    }
    return "denied"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Int(1));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "approved"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_multiclass() {
    let src = r#"
flow test() -> string {
    category = llm.classify(
        model: "m",
        prompt: "Classify this error",
        categories: ["syntax", "runtime", "logic"],
    )
    when category == "runtime" {
        return "runtime error"
    }
    return "other: " + category
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("runtime".into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "runtime error"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_retries_explanatory_prose_then_accepts_explicit_label() {
    let src = r#"
flow test() -> string {
    return llm.classify(
        model: "m",
        prompt: "Judge the agent state",
        categories: ["forgot_tools", "lazy", "done"],
        retry: 1,
    )
}
"#;
    let retry_prefix = "Answer with exactly one of these labels: forgot_tools, lazy, done.\n\nJudge the agent state\n\nYour previous response could not be parsed.";
    let provider = MockProvider::new("mock")
        .with_prefix("m", retry_prefix, Value::Str("done".into()))
        .with_model(
            "m",
            Value::Str("The agent already used tools and completed the task.".into()),
        );
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "done"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn classify_binary_parse_fail_returns_false() {
    // LLM returns unparseable text → binary classify should return Bool(false)
    let src = r#"
flow test() -> string {
    when llm.classify(model: "m", prompt: "safe?") {
        return "approved"
    }
    return "denied (fail-safe)"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("maybe perhaps".into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "denied (fail-safe)"),
        other => panic!("expected string, got {other:?}"),
    }
}

// === llm.extract ===

#[test]
fn extract_basic() {
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract from text",
        fields: {
            valid: bool -- "is valid",
            count: int -- "how many",
        },
    )
    when result.valid == true {
        return "valid: " + to_json_string(result.count)
    }
    return "invalid"
}
"#;
    let json = r#"{"valid": true, "count": 42}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "valid: 42"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn extract_with_code_fence() {
    // LLM wraps JSON in markdown code fence
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract",
        fields: {
            status: string -- "status",
        },
    )
    return result.status
}
"#;
    let response = "```json\n{\"status\": \"ok\"}\n```";
    let provider = MockProvider::new("mock").with_model("m", Value::Str(response.into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "ok"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn extract_coerce_bool_string() {
    // LLM returns {"valid": "true"} (string instead of bool) → should coerce
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract",
        fields: {
            valid: bool -- "is valid",
        },
    )
    when result.valid == true {
        return "coerced ok"
    }
    return "coerce failed"
}
"#;
    let json = r#"{"valid": "true"}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "coerced ok"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn extract_pre_parsed_struct() {
    // assistant_message_to_value may pre-parse JSON to Value::Struct
    // MockProvider with_model stores a Value, and dispatch_llm returns it via assistant_message_to_value
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract",
        fields: {
            name: string -- "name",
        },
    )
    return result.name
}
"#;
    // When mock returns a Value::Str that is valid JSON, assistant_message_to_value
    // parses it to Value::Struct. So we pass JSON string.
    let json = r#"{"name": "hello"}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "hello"),
        other => panic!("expected string, got {other:?}"),
    }
}

// === llm.generate_branches ===

#[test]
fn generate_branches_json_array() {
    let src = r#"
flow test() -> string {
    tasks = llm.generate_branches(
        model: "m",
        prompt: "Break down: review code",
    )
    return to_json_string(tasks)
}
"#;
    let json = r#"["review tests", "review types", "review logic"]"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => assert!(s.contains("review tests") && s.contains("review types")),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn generate_branches_newline_fallback() {
    // LLM returns plain text list instead of JSON array
    let src = r#"
flow test() -> string {
    tasks = llm.generate_branches(
        model: "m",
        prompt: "Break down",
    )
    return to_json_string(tasks)
}
"#;
    let response = "- Review tests\n- Review types\n- Review logic";
    let provider = MockProvider::new("mock").with_model("m", Value::Str(response.into()));
    match run(src, provider) {
        Value::Str(s) => assert!(s.contains("Review tests") && s.contains("Review types")),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn generate_branches_with_count() {
    let src = r#"
flow test() -> string {
    tasks = llm.generate_branches(
        model: "m",
        prompt: "Break down",
        count: 2,
    )
    return to_json_string(tasks)
}
"#;
    let json = r#"["task1", "task2", "task3", "task4"]"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => {
            assert!(s.contains("task1") && s.contains("task2") && !s.contains("task3"))
        }
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn extract_with_annotation_syntax() {
    // New `--` annotation syntax for fields
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract from text",
        fields: {
            valid: bool -- "is valid",
            count: int -- "how many",
        },
    )
    when result.valid == true {
        return "valid: " + to_json_string(result.count)
    }
    return "invalid"
}
"#;
    let json = r#"{"valid": true, "count": 42}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => assert_eq!(s, "valid: 42"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn extract_with_list_type_annotation() {
    let src = r#"
flow test() -> string {
    result = llm.extract(
        model: "m",
        prompt: "Extract",
        fields: {
            issues: [string] -- "list of issues",
        },
    )
    return to_json_string(result.issues)
}
"#;
    let json = r#"{"issues": ["bug1", "bug2"]}"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str(json.into()));
    match run(src, provider) {
        Value::Str(s) => {
            assert!(s.contains("bug1"), "should contain bug1: {s}");
            assert!(s.contains("bug2"), "should contain bug2: {s}");
        }
        other => panic!("expected string, got {other:?}"),
    }
}
