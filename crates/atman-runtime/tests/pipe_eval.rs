use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

#[tokio::test]
async fn pipe_prepends_lhs_as_first_positional_arg() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    std::fs::write(&f, "hello world\n").unwrap();

    let src = format!(
        r#"flow t() -> int {{
    n = fs.read("{}") |> len()
    return n
}}
"#,
        f.display()
    );

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let file = parse_file(&src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Int(n) => assert_eq!(n as usize, "hello world\n".len()),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn pipe_chains_left_to_right() {
    let src = r#"flow t() -> int {
    result = [1, 2, 3] |> len() |> to_json_string() |> len()
    return result
}
"#;
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Int(n) => assert_eq!(n, 1, "to_json_string of len(list) = \"3\", len(\"3\") = 1"),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn pipe_with_extra_args_appends_after_lhs() {
    let src = r#"flow t() -> list {
    out = [1, 2] |> concat([3, 4])
    return out
}
"#;
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
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
