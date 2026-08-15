use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

async fn run(src: &str) -> Value {
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    ex.run(&file, "start", vec![]).await.unwrap()
}

#[tokio::test]
async fn estimate_tokens_returns_int_for_message_list() {
    let out = run(r#"flow start() -> int {
    msgs = [user_msg("hello world"), assistant_msg("hi there")]
    return estimate_tokens(msgs)
}"#)
    .await;
    let Value::Int(n) = out else {
        panic!("expected int, got {out:?}");
    };
    assert!(n > 0);
}

#[tokio::test]
async fn find_compact_range_reports_not_found_when_under_budget() {
    let out = run(r#"flow start() -> bool {
    msgs = [user_msg("a"), assistant_msg("b")]
    range = find_compact_range(msgs, 10000)
    return range.found
}"#)
    .await;
    assert!(matches!(out, Value::Bool(false)));
}

#[tokio::test]
async fn full_compaction_pipeline_shrinks_message_list() {
    let out = run(
        r#"flow start() -> int {
    msgs = [
        system_msg("head"),
        user_msg("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        assistant_msg("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        user_msg("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
        assistant_msg("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"),
        user_msg("old-1"),
        assistant_msg("old-2"),
        user_msg("old-3"),
        assistant_msg("old-4"),
        user_msg("old-5"),
        assistant_msg("old-6"),
        user_msg("old-7"),
        assistant_msg("old-8"),
        user_msg("old-9"),
        assistant_msg("old-10"),
        user_msg("tail-1"),
        assistant_msg("tail-2"),
    ]
    range = find_compact_range(msgs, 20)
    when range.found == false {
        return -1
    }
    compacted = replace_messages_range(msgs, range.start, range.end, "middle turns discussed X and Y")
    return len(compacted)
}"#,
    )
    .await;
    let Value::Int(n) = out else {
        panic!("expected int, got {out:?}");
    };
    assert!(n >= 0, "expected a compactible range, got {n}");
    assert!(
        n < 17,
        "compacted must be smaller than the original 17, got {n}"
    );
    assert!(n >= 3, "must retain head + summary + tail, got {n}");
}
