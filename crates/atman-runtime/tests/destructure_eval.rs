use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

#[tokio::test]
async fn destructure_binds_each_field_into_scope() {
    let src = r#"flow t() -> string {
    { status, body } = { status: "ok", body: "hello" }
    return body
}
"#;
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Str(s) => assert_eq!(s, "hello"),
        other => panic!("expected str, got {other:?}"),
    }
}

#[tokio::test]
async fn destructure_supports_rename_into_new_name() {
    let src = r#"flow t() -> string {
    { error: err } = { error: "oops" }
    return err
}
"#;
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Str(s) => assert_eq!(s, "oops"),
        other => panic!("expected str, got {other:?}"),
    }
}

#[tokio::test]
async fn destructure_missing_field_reports_missing_arg() {
    let src = r#"flow t() -> string {
    { nope } = { present: "yes" }
    return nope
}
"#;
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let file = parse_file(src).unwrap();
    let err = ex.run(&file, "t", vec![]).await.expect_err("should error");
    let msg = format!("{err}");
    assert!(
        msg.contains("nope") || msg.contains("field"),
        "want missing-field error, got: {msg}"
    );
}
