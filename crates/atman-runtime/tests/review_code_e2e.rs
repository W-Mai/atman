use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tools::memory_stubs::RuleFetch;
use atman_runtime::{Executor, Value, tools};

static TEST_CFG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install_test_models() {
    use atman_runtime::model_registry::{ModelConfig, ModelEntry};

    let models = ["claude-opus-4.7", "gpt-4o-mini"]
        .into_iter()
        .map(|model| {
            (
                model.to_string(),
                ModelEntry {
                    model: model.to_string(),
                    provider: Some("mock".into()),
                    context_budget: Some(8_192),
                    ..Default::default()
                },
            )
        })
        .collect();
    atman_runtime::model_registry::set_model_config(ModelConfig {
        models,
        providers: std::collections::HashMap::new(),
        aliases: std::collections::HashMap::new(),
    });
}

const REVIEW_FLOW: &str = r#"flow review_code(file: path) -> Review {
    gather = fanout [
        rule.fetch("code-review"),
        rule.fetch(query: "none"),
    ] collect: all

    primary = llm.call(
        model: "claude-opus-4.7",
        prompt: "review this code",
        input: gather,
    )

    verify = llm.call(
        model: "gpt-4o-mini",
        prompt: "verify this review",
        input: primary,
    )

    when verify.valid == false {
        primary = llm.call(
            model: "claude-opus-4.7",
            prompt: "review again",
            input: gather,
        )
    }

    when primary.severity == "critical" {
        user_confirm("critical issue, proceed?")
    }

    return primary
}
"#;

#[test]
fn examples_review_code_at_parses() {
    let src = std::fs::read_to_string("../../examples/review_code.at").unwrap();
    let file = parse_file(&src).unwrap();
    assert_eq!(file.flows.len(), 1);
    assert_eq!(file.flows[0].name.name, "review_code");
    let contract = file.flows[0]
        .contract
        .as_ref()
        .expect("upgraded flow must declare contract");
    assert!(contract.blocks.iter().any(|b| b.name.name == "scope"));
}

#[tokio::test]
async fn end_to_end_review_flow_produces_structured_output() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_test_models();
    let file = parse_file(REVIEW_FLOW).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

    let rule = RuleFetch::new();
    rule.insert("code-review", "review carefully, look for as-any")
        .await;
    ex.tools.register(Arc::new(rule));

    let review = Value::Struct(vec![
        ("severity".into(), Value::Str("info".into())),
        ("issues".into(), Value::List(vec![])),
    ]);
    let verdict = Value::Struct(vec![
        ("valid".into(), Value::Bool(true)),
        ("issues".into(), Value::List(vec![])),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_model("claude-opus-4.7", review)
            .with_model("gpt-4o-mini", verdict),
    ));

    let out = ex
        .run(
            &file,
            "review_code",
            vec![("file".into(), Value::Path("src/main.rs".into()))],
        )
        .await
        .unwrap();
    if let Value::Struct(fields) = out {
        let names: Vec<_> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["severity", "issues"]);
        assert!(matches!(&fields[0].1, Value::Str(s) if s == "info"));
    } else {
        panic!("expected struct output, got {out:?}");
    }
    let events = ex.events.snapshot();
    assert!(events.len() >= 2);
    assert!(matches!(
        events.first(),
        Some(atman_runtime::Event::FlowStart { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(atman_runtime::Event::FlowEnd { .. })
    ));
}

#[tokio::test]
async fn retry_branch_fires_when_verify_reports_invalid() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_test_models();
    let file = parse_file(REVIEW_FLOW).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    ex.tools.register(Arc::new(RuleFetch::new()));

    let bad = Value::Struct(vec![("severity".into(), Value::Str("info".into()))]);
    let good = Value::Struct(vec![("severity".into(), Value::Str("critical".into()))]);
    let verdict = Value::Struct(vec![
        ("valid".into(), Value::Bool(false)),
        ("issues".into(), Value::List(vec![])),
    ]);
    let mp = MockProvider::new("mock")
        .with_prefix("claude-opus-4.7", "review this code", bad)
        .with_prefix("claude-opus-4.7", "review again", good)
        .with_model("gpt-4o-mini", verdict);
    ex.providers.register(Arc::new(mp));

    let out = ex
        .run(
            &file,
            "review_code",
            vec![("file".into(), Value::Path("src/main.rs".into()))],
        )
        .await
        .unwrap();
    if let Value::Struct(fields) = out {
        assert!(matches!(&fields[0].1, Value::Str(s) if s == "critical"));
    } else {
        panic!("expected struct");
    }
}
