use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, RuntimeError, Value, tools};

#[tokio::test(flavor = "multi_thread")]
async fn watch_aborts_bash_stream_on_token_match_before_full_output() {
    let src = r#"flow t() -> string {
    contract {
        capabilities { shell: true }
    }
    run = bash.exec(cmd: "for i in 1 2 3 4 5; do echo tick_$i; sleep 0.1; done")
    watch run {
        on token(match: "tick_3") {
            abort("saw tick_3")
        }
    }
    return "unused"
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    ex.providers.register(Arc::new(MockProvider::new("mock")));

    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    match err {
        RuntimeError::Aborted(msg) => assert!(msg.contains("tick_3"), "msg: {msg}"),
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_does_not_fire_on_clean_bash_output_and_returns_full_stdout() {
    let src = r#"flow t() -> string {
    contract {
        capabilities { shell: true }
    }
    run = bash.exec(cmd: "for i in 1 2 3; do echo tock_$i; done")
    watch run {
        on token(match: "never_appears_in_output") {
            abort("bogus")
        }
    }
    return run.stdout
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    ex.providers.register(Arc::new(MockProvider::new("mock")));

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    match out {
        Value::Str(s) => {
            assert!(s.contains("tock_1"), "stdout: {s}");
            assert!(s.contains("tock_2"), "stdout: {s}");
            assert!(s.contains("tock_3"), "stdout: {s}");
        }
        other => panic!("expected str, got {other:?}"),
    }
}
