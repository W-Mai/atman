use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value};

#[test]
fn examples_look_into_at_parses_with_subflow() {
    let src = std::fs::read_to_string("../../examples/look_into.at").unwrap();
    let file = parse_file(&src).unwrap();
    let names: Vec<_> = file.flows.iter().map(|f| f.name.name.as_str()).collect();
    assert!(names.contains(&"look_into"));
    assert!(names.contains(&"explore_module"));
}

#[tokio::test]
async fn look_into_fanout_subflow_synthesizes_via_mock_providers() {
    // Inline flow avoids `@"..."` FileRef which resolves against process CWD.
    const FLOW: &str = r#"flow look_into(question: string) -> Report {
    findings = fanout [
        subflow(explore_module, question, "src/"),
        subflow(explore_module, question, "tests/"),
    ] collect: all

    summary = llm {
        model: "claude-opus-4.7"
        prompt: "synthesize"
        input: { question: question, findings: findings }
        schema: Report
    }

    return summary
}

flow explore_module(question: string, dir: string) -> Finding {
    return llm {
        model: "gpt-4o-mini"
        prompt: "explore"
        schema: Finding
    }
}
"#;

    let file = parse_file(FLOW).unwrap();
    let mut ex = Executor::new();

    let finding = Value::Struct(vec![
        ("module".into(), Value::Str("src".into())),
        ("quote".into(), Value::Str("fn main() {}".into())),
    ]);
    let report = Value::Struct(vec![
        ("verdict".into(), Value::Str("entrypoint is main".into())),
        ("evidence".into(), Value::List(vec![])),
        ("gaps".into(), Value::List(vec![])),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_model("gpt-4o-mini", finding)
            .with_model("claude-opus-4.7", report),
    ));

    let out = ex
        .run(
            &file,
            "look_into",
            vec![("question".into(), Value::Str("where is main?".into()))],
        )
        .await
        .unwrap();
    let Value::Struct(fields) = out else {
        panic!("expected struct, got {out:?}");
    };
    let verdict = fields
        .iter()
        .find(|(k, _)| k == "verdict")
        .map(|(_, v)| v.clone())
        .expect("verdict field missing");
    assert!(matches!(verdict, Value::Str(s) if s == "entrypoint is main"));
}
