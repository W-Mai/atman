use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

async fn run(src: &str) -> Value {
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.run(&file, "start", vec![]).await.unwrap()
}

#[tokio::test]
async fn sub_mul_div_mod_arithmetic() {
    let out = run(r#"
flow start() -> int {
    return 10 - 3
}
"#)
    .await;
    assert!(matches!(out, Value::Int(7)));

    let out = run(r#"
flow start() -> int {
    return 7 * 6
}
"#)
    .await;
    assert!(matches!(out, Value::Int(42)));

    let out = run(r#"
flow start() -> int {
    return 20 / 4
}
"#)
    .await;
    assert!(matches!(out, Value::Int(5)));

    let out = run(r#"
flow start() -> int {
    return 17 % 5
}
"#)
    .await;
    assert!(matches!(out, Value::Int(2)));
}

#[tokio::test]
async fn precedence_mul_binds_tighter_than_add() {
    let out = run(r#"
flow start() -> int {
    return 2 + 3 * 4
}
"#)
    .await;
    assert!(matches!(out, Value::Int(14)));
}

#[tokio::test]
async fn unary_neg_and_not() {
    let out = run(r#"
flow start() -> int {
    x = 5
    return -x
}
"#)
    .await;
    assert!(matches!(out, Value::Int(-5)));

    let out = run(r#"
flow start() -> bool {
    return !false
}
"#)
    .await;
    assert!(matches!(out, Value::Bool(true)));
}

#[tokio::test]
async fn divide_by_zero_returns_err() {
    let file = parse_file(
        r#"
flow start() -> int {
    return 1 / 0
}
"#,
    )
    .unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let err = ex.run(&file, "start", vec![]).await.unwrap_err();
    assert!(format!("{err}").contains("div by zero"));
}

#[tokio::test]
async fn recursive_countdown_with_subtraction() {
    let out = run(r#"
flow countdown(n: int) -> int {
    when n <= 0 {
        return 0
    }
    return subflow(countdown, n - 1)
}

flow start() -> int {
    return subflow(countdown, 10)
}
"#)
    .await;
    assert!(matches!(out, Value::Int(0)));
}

#[tokio::test]
async fn list_head_tail_len_is_empty() {
    let out = run(r#"
flow start() -> int {
    xs = [10, 20, 30, 40]
    return len(xs)
}
"#)
    .await;
    assert!(matches!(out, Value::Int(4)));

    let out = run(r#"
flow start() -> int {
    return head([100, 200, 300])
}
"#)
    .await;
    assert!(matches!(out, Value::Int(100)));

    let out = run(r#"
flow start() -> int {
    return len(tail([1, 2, 3, 4]))
}
"#)
    .await;
    assert!(matches!(out, Value::Int(3)));

    let out = run(r#"
flow start() -> bool {
    return is_empty([])
}
"#)
    .await;
    assert!(matches!(out, Value::Bool(true)));
}

#[tokio::test]
async fn recursive_sum_over_list_via_head_tail() {
    let out = run(r#"
flow sum(xs: [int]) -> int {
    when is_empty(xs) {
        return 0
    }
    return head(xs) + subflow(sum, tail(xs))
}

flow start() -> int {
    return subflow(sum, [1, 2, 3, 4, 5])
}
"#)
    .await;
    assert!(matches!(out, Value::Int(15)));
}
