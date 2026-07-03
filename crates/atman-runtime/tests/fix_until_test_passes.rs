use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

fn contract_shell() -> &'static str {
    r#"contract { scope { read: [project_root] write: [project_root] } capabilities { shell: true } }"#
}

#[tokio::test]
async fn passes_immediately_on_first_iter_when_test_exits_zero() {
    let src = format!(
        r#"flow demo() -> string {{
    {contract}
    result = fix_until_test_passes {{
        edit_flow: "noop"
        test: bash.exec("true")
        max_iters: 5
    }}
    return result.status
}}
"#,
        contract = contract_shell()
    );
    let file = parse_file(&src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "demo", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "passed"));
}

#[tokio::test]
async fn gives_up_after_max_iters_when_test_never_passes() {
    let src = format!(
        r#"flow demo() -> string {{
    {contract}
    result = fix_until_test_passes {{
        edit_flow: "noop"
        test: bash.exec("false")
        max_iters: 3
    }}
    return result.status
}}
"#,
        contract = contract_shell()
    );
    let file = parse_file(&src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "demo", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "gave_up"));
}

#[tokio::test]
async fn recovers_after_two_failures_then_passes() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("counter");
    let script = format!(
        "F={}; N=$(cat $F 2>/dev/null || echo 0); echo $((N+1)) > $F; if [ $N -lt 2 ]; then exit 1; else exit 0; fi",
        counter.display()
    );
    let src = format!(
        r#"flow demo() -> int {{
    {contract}
    result = fix_until_test_passes {{
        edit_flow: "noop"
        test: bash.exec("{script}")
        max_iters: 5
    }}
    return result.iters
}}
"#,
        contract = contract_shell(),
        script = script.replace('"', "\\\"")
    );
    let file = parse_file(&src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "demo", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Int(3)), "want 3 iters, got {out:?}");
}

#[tokio::test]
async fn on_giveup_runs_when_max_iters_exhausted() {
    let src = format!(
        r#"flow demo() -> string {{
    {contract}
    result = fix_until_test_passes {{
        edit_flow: "noop"
        test: bash.exec("false")
        max_iters: 2
        on_giveup: "fallback-triggered"
    }}
    return result
}}
"#,
        contract = contract_shell()
    );
    let file = parse_file(&src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "demo", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "fallback-triggered"));
}

#[tokio::test]
async fn iter_and_iters_variables_available_in_edit_and_giveup_scopes() {
    let src = format!(
        r#"flow demo() -> int {{
    {contract}
    result = fix_until_test_passes {{
        edit_flow: iter
        test: bash.exec("false")
        max_iters: 3
        on_giveup: iters
    }}
    return result
}}
"#,
        contract = contract_shell()
    );
    let file = parse_file(&src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "demo", vec![]).await.unwrap();
    assert!(
        matches!(&out, Value::Int(3)),
        "on_giveup should see iters==3, got {out:?}"
    );
}
