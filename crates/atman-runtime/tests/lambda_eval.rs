use atman_rt::parse_file;
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;
use atman_runtime::Executor;

fn run(src: &str) -> Value {
    let parsed = parse_file(src).expect("parse");
    let ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(ex.run(&parsed, "test", vec![]))
        .expect("flow failed")
}

fn run_result(src: &str) -> Result<Value, atman_runtime::error::RuntimeError> {
    let parsed = parse_file(src).expect("parse");
    let ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(ex.run(&parsed, "test", vec![]))
}

fn run_str(src: &str) -> String {
    match run(src) {
        Value::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    }
}

// === list.map ===

#[test]
fn map_basic() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.map([1, 2, 3], |x| x * 2)) }"#
        ),
        "[\n  2,\n  4,\n  6\n]"
    );
}

#[test]
fn map_empty() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.map([], |x| x * 2)) }"#),
        "[]"
    );
}

#[test]
fn map_single() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.map([5], |x| x * 2)) }"#),
        "[\n  10\n]"
    );
}

#[test]
fn map_identity() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.map([1, 2, 3], |x| x)) }"#),
        "[\n  1,\n  2,\n  3\n]"
    );
}

// === list.filter ===

#[test]
fn filter_basic() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.filter([1, 2, 3, 4], |x| x > 2)) }"#
        ),
        "[\n  3,\n  4\n]"
    );
}

#[test]
fn filter_empty() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.filter([], |x| x > 0)) }"#),
        "[]"
    );
}

#[test]
fn filter_all_removed() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.filter([1, 2, 3], |x| x > 10)) }"#
        ),
        "[]"
    );
}

#[test]
fn filter_all_kept() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.filter([1, 2, 3], |x| x > 0)) }"#
        ),
        "[\n  1,\n  2,\n  3\n]"
    );
}

// === list.reduce ===

#[test]
fn reduce_sum() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.reduce([1, 2, 3], |acc, x| acc + x, 0)) }"#
        ),
        "6"
    );
}

#[test]
fn reduce_empty_returns_init() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.reduce([], |acc, x| acc + x, 42)) }"#
        ),
        "42"
    );
}

#[test]
fn reduce_single() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.reduce([5], |acc, x| acc + x, 0)) }"#
        ),
        "5"
    );
}

#[test]
fn reduce_string_concat() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return list.reduce(["a", "b"], |acc, x| acc + x, "") }"#
        ),
        "ab"
    );
}

#[test]
fn reduce_multiply() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.reduce([1, 2, 3], |acc, x| acc * x, 1)) }"#
        ),
        "6"
    );
}

// === list.find ===

#[test]
fn find_match() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.find([1, 2, 3], |x| x == 2)) }"#
        ),
        "2"
    );
}

#[test]
fn find_no_match() {
    match run(
        r#"flow test() -> string { return to_json_string(list.find([1, 2, 3], |x| x == 99)) }"#,
    ) {
        Value::Str(s) => assert_eq!(s, "null"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn find_empty() {
    match run(r#"flow test() -> string { return to_json_string(list.find([], |x| x > 0)) }"#) {
        Value::Str(s) => assert_eq!(s, "null"),
        other => panic!("expected string, got {other:?}"),
    }
}

// === list.any ===

#[test]
fn any_true() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.any([1, 2, 3], |x| x > 2)) }"#
        ),
        "true"
    );
}

#[test]
fn any_false() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.any([1, 2, 3], |x| x > 10)) }"#
        ),
        "false"
    );
}

#[test]
fn any_empty() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.any([], |x| x > 0)) }"#),
        "false"
    );
}

// === list.all ===

#[test]
fn all_true() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.all([1, 2, 3], |x| x > 0)) }"#
        ),
        "true"
    );
}

#[test]
fn all_false() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.all([1, 2, 3], |x| x > 1)) }"#
        ),
        "false"
    );
}

#[test]
fn all_empty() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(list.all([], |x| x > 0)) }"#),
        "true"
    );
}

// === Closure capture ===

#[test]
fn closure_capture_basic() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { n = 5 return to_json_string(list.filter([1, 2, 3, 4, 5, 6, 7], |x| x > n)) }"#
        ),
        "[\n  6,\n  7\n]"
    );
}

#[test]
fn closure_capture_multiple() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { min_val = 2 max_val = 5 return to_json_string(list.filter([1, 2, 3, 4, 5, 6], |x| x >= min_val && x <= max_val)) }"#
        ),
        "[\n  2,\n  3,\n  4,\n  5\n]"
    );
}

#[test]
fn closure_capture_struct_field() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { config = { threshold: 10 } return to_json_string(list.filter([5, 10, 15, 20], |x| x >= config.threshold)) }"#
        ),
        "[\n  10,\n  15,\n  20\n]"
    );
}

#[test]
fn closure_param_shadows_outer() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { x = 100 return to_json_string(list.map([1, 2, 3], |x| x * 2)) }"#
        ),
        "[\n  2,\n  4,\n  6\n]"
    );
}

#[test]
fn nested_lambda() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.map([1, 2, 3], |outer| list.map([10, 20], |inner| outer + inner))) }"#
        ),
        "[\n  [\n    11,\n    21\n  ],\n  [\n    12,\n    22\n  ],\n  [\n    13,\n    23\n  ]\n]"
    );
}

// === Complex combinations ===

#[test]
fn chain_map_filter() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.filter(list.map([1, 2, 3, 4, 5], |x| x * x), |x| x > 10)) }"#
        ),
        "[\n  16,\n  25\n]"
    );
}

#[test]
fn map_reduce() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.reduce(list.map([1, 2, 3], |x| x * 2), |acc, x| acc + x, 0)) }"#
        ),
        "12"
    );
}

#[test]
fn filter_find() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.find(list.filter([1, 2, 3, 4, 5, 6], |x| x > 2), |x| x > 4)) }"#
        ),
        "5"
    );
}

#[test]
fn three_layer_nested() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.map(list.filter(list.map([1, 2, 3, 4, 5], |x| x * 2), |x| x > 4), |x| x + 1)) }"#
        ),
        "[\n  7,\n  9,\n  11\n]"
    );
}

#[test]
fn any_map() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.any(list.map([1, 2, 3], |x| x * 2), |x| x > 4)) }"#
        ),
        "true"
    );
}

#[test]
fn reduce_string_build() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return list.reduce(list.map(["hello", "world"], |s| s + "!"), |acc, s| acc + " " + s, "") }"#
        ),
        " hello! world!"
    );
}

// === Dynamic fanout ===

#[test]
fn fanout_basic() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(fanout ["a", "b", "c"] { |t| t + "!" }) }"#
        ),
        "[\n  \"a!\",\n  \"b!\",\n  \"c!\"\n]"
    );
}

#[test]
fn fanout_empty() {
    assert_eq!(
        run_str(r#"flow test() -> string { return to_json_string(fanout [] { |t| t }) }"#),
        "[]"
    );
}

#[test]
fn fanout_accepts_array_variable() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { pending = [1, 2, 3] return to_json_string(fanout pending) }"#
        ),
        "[\n  1,\n  2,\n  3\n]"
    );
    assert_eq!(
        run_str(
            r#"flow test() -> string { pending = [1, 2, 3] return to_json_string(len(fanout pending)) }"#
        ),
        "3"
    );
}

#[test]
fn fanout_rejects_non_array_source() {
    let error = run_result(r#"flow test() { pending = 7 return fanout pending }"#)
        .expect_err("non-array source must fail");
    assert!(error.to_string().contains("list"), "{error}");
}

#[test]
fn fanout_closure_capture() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { prefix = "done_" return to_json_string(fanout ["a", "b"] { |t| prefix + t }) }"#
        ),
        "[\n  \"done_a\",\n  \"done_b\"\n]"
    );
}

#[test]
fn fanout_chain_map() {
    assert_eq!(
        run_str(
            r#"flow test() -> string { return to_json_string(list.map(fanout ["a", "b"] { |t| t + "!" }, |r| r + "?")) }"#
        ),
        "[\n  \"a!?\",\n  \"b!?\"\n]"
    );
}

// === Error handling ===
#[test]
fn error_map_non_list() {
    assert!(
        run_result(
            r#"flow test() -> string { return to_json_string(list.map("not a list", |x| x)) }"#
        )
        .is_err(),
        "should return error for non-list"
    );
}

#[test]
fn error_map_non_lambda() {
    assert!(
        run_result(
            r#"flow test() -> string { return to_json_string(list.map([1, 2], "not a lambda")) }"#
        )
        .is_err(),
        "should return error for non-lambda"
    );
}

#[test]
fn error_arity_map() {
    assert!(matches!(
        run_result(
            r#"flow test() -> string { return to_json_string(list.map([1, 2], |x, y| x + y)) }"#
        ),
        Err(atman_runtime::error::RuntimeError::ToolFailed(message))
            if message == "list.map: lambda must have 1 parameter, got 2"
    ));
}

#[test]
fn error_arity_reduce() {
    assert!(
        run_result(
            r#"flow test() -> string { return to_json_string(list.reduce([1, 2], |x| x, 0)) }"#
        )
        .is_err(),
        "should return error for wrong arity"
    );
}

#[test]
fn error_missing_lambda_arg() {
    assert!(matches!(
        run_result(r#"flow test() -> string { return to_json_string(list.map([1, 2])) }"#),
        Err(atman_runtime::error::RuntimeError::MissingArg(message))
            if message == "list.map: missing positional arg 1"
    ));
}

#[test]
fn error_missing_init_arg() {
    assert!(run_result(r#"flow test() -> string { return to_json_string(list.reduce([1, 2], |acc, x| acc + x)) }"#).is_err(), "should return error for missing init");
}
