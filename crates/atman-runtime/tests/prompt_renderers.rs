use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

async fn eval(src: &str) -> Value {
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.run(&file, "start", vec![]).await.unwrap()
}

#[tokio::test]
async fn xml_renderer_wraps_fields_in_xml_tags() {
    let out = eval(
        r#"flow start() -> string {
    return render_prompt_xml({
        role: "code reviewer",
        task: "review this file",
    })
}"#,
    )
    .await;
    let Value::Str(s) = out else {
        panic!("expected string");
    };
    assert!(s.contains("<role>code reviewer</role>"), "got: {s}");
    assert!(s.contains("<task>review this file</task>"), "got: {s}");
}

#[tokio::test]
async fn markdown_renderer_uses_headers() {
    let out = eval(
        r#"flow start() -> string {
    return render_prompt_markdown({
        role: "senior engineer",
        task: "audit this",
    })
}"#,
    )
    .await;
    let Value::Str(s) = out else {
        panic!("expected string");
    };
    assert!(s.contains("# Role\nsenior engineer"), "got: {s}");
    assert!(s.contains("# Task\naudit this"), "got: {s}");
}

#[tokio::test]
async fn terse_renderer_omits_decoration() {
    let out = eval(
        r#"flow start() -> string {
    return render_prompt_terse({
        role: "local model",
        task: "classify"
    })
}"#,
    )
    .await;
    let Value::Str(s) = out else {
        panic!("expected string");
    };
    assert!(s.contains("Role: local model"), "got: {s}");
    assert!(s.contains("Task: classify"), "got: {s}");
    assert!(!s.contains("<"), "no XML tags in terse: {s}");
    assert!(!s.contains("#"), "no markdown headers in terse: {s}");
}

#[tokio::test]
async fn renderers_include_context_and_schema_when_provided() {
    let out = eval(
        r#"flow start() -> string {
    return render_prompt_xml({
        role: "reviewer",
        context: { file: "main.rs", lines: 42 },
        task: "check",
        schema: "Review"
    })
}"#,
    )
    .await;
    let Value::Str(s) = out else {
        panic!("expected string");
    };
    assert!(s.contains("<context>"), "got: {s}");
    assert!(s.contains("main.rs"), "got: {s}");
    assert!(s.contains("<schema>Review</schema>"), "got: {s}");
}

#[tokio::test]
async fn renderers_handle_examples_list() {
    let out = eval(
        r#"flow start() -> string {
    return render_prompt_markdown({
        role: "translator",
        task: "translate to French",
        examples: [
            { input: "hello", output: "bonjour" },
            { input: "world", output: "monde" },
        ]
    })
}"#,
    )
    .await;
    let Value::Str(s) = out else {
        panic!("expected string");
    };
    assert!(s.contains("# Examples"), "got: {s}");
    assert!(s.contains("hello"), "got: {s}");
    assert!(s.contains("bonjour"), "got: {s}");
}
