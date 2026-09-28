use atman_rt::parse_file;
use atman_runtime::{Executor, tools};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

#[tokio::test]
async fn explicit_binding_passes_result_to_tool() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    std::fs::write(&f, "hello world\n").unwrap();

    let src = format!(
        r#"flow t() -> int {{
    contents = fs.read("{}")
    n = len(contents)
    return n
}}
"#,
        f.display()
    );

    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let file = parse_file(&src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Int(n) => assert_eq!(n as usize, "hello world\n".len()),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn explicit_bindings_chain_tool_calls() {
    let src = r#"flow t() -> int {
    items = [1, 2, 3]
    count = len(items)
    encoded = to_json_string(count)
    result = len(encoded)
    return result
}
"#;
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Int(n) => assert_eq!(n, 1, "to_json_string of len(list) = \"3\", len(\"3\") = 1"),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn explicit_binding_preserves_argument_order() {
    let src = r#"flow t() -> list {
    left = [1, 2]
    out = concat(left, [3, 4])
    return out
}
"#;
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    let list = match val {
        Value::List(xs) => xs,
        other => panic!("expected list, got {other:?}"),
    };
    let ints: Vec<i64> = list
        .into_iter()
        .map(|v| match v {
            Value::Int(n) => n,
            other => panic!("want int, got {other:?}"),
        })
        .collect();
    assert_eq!(ints, vec![1, 2, 3, 4]);
}
